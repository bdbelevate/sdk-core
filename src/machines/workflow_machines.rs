use crate::{
    machines::{
        activity_state_machine::new_activity, cancel_external_state_machine::new_external_cancel,
        cancel_workflow_state_machine::cancel_workflow,
        child_workflow_state_machine::new_child_workflow,
        complete_workflow_state_machine::complete_workflow,
        continue_as_new_workflow_state_machine::continue_as_new,
        fail_workflow_state_machine::fail_workflow, patch_state_machine::has_change,
        signal_external_state_machine::new_external_signal, timer_state_machine::new_timer,
        workflow_task_state_machine::WorkflowTaskMachine, MachineKind, NewMachineWithCommand,
        ProtoCommand, TemporalStateMachine, WFCommand,
    },
    protosext::HistoryEventExt,
    telemetry::{metrics::MetricsContext, VecDisplayer},
    workflow::{CommandID, DrivenWorkflow, HistoryUpdate, WorkflowFetcher},
};
use prost_types::TimestampOutOfSystemRangeError;
use slotmap::SlotMap;
use std::{
    borrow::{Borrow, BorrowMut},
    collections::{hash_map::DefaultHasher, HashMap, VecDeque},
    convert::TryInto,
    hash::{Hash, Hasher},
    time::{Duration, Instant, SystemTime},
};
use temporal_sdk_core_protos::{
    coresdk::{
        common::{NamespacedWorkflowExecution, Payload},
        workflow_activation::{
            wf_activation_job::{self, Variant},
            NotifyHasPatch, StartWorkflow, UpdateRandomSeed, WfActivation,
        },
        workflow_commands::{
            request_cancel_external_workflow_execution as cancel_we,
            signal_external_workflow_execution as sig_we,
        },
        FromPayloadsExt,
    },
    temporal::api::{
        common::v1::Header,
        enums::v1::EventType,
        history::v1::{history_event, HistoryEvent},
    },
};
use tracing::Level;

type Result<T, E = WFMachinesError> = std::result::Result<T, E>;

slotmap::new_key_type! { struct MachineKey; }
/// Handles all the logic for driving a workflow. It orchestrates many state machines that together
/// comprise the logic of an executing workflow. One instance will exist per currently executing
/// (or cached) workflow on the worker.
pub(crate) struct WorkflowMachines {
    /// The last recorded history we received from the server for this workflow run. This must be
    /// kept because the lang side polls & completes for every workflow task, but we do not need
    /// to poll the server that often during replay.
    last_history_from_server: HistoryUpdate,
    /// EventId of the last handled WorkflowTaskStarted event
    current_started_event_id: i64,
    /// The event id of the next workflow task started event that the machines need to process.
    /// Eventually, this number should reach the started id in the latest history update, but
    /// we must incrementally apply the history while communicating with lang.
    next_started_event_id: i64,
    /// True if the workflow is replaying from history
    pub replaying: bool,
    /// Namespace this workflow exists in
    pub namespace: String,
    /// Workflow identifier
    pub workflow_id: String,
    /// Identifies the current run
    pub run_id: String,
    /// The time the workflow execution began, as told by the WEStarted event
    workflow_start_time: Option<SystemTime>,
    /// The time the workflow execution finished, as determined by when the machines handled
    /// a terminal workflow command. If this is `Some`, you know the workflow is ended.
    workflow_end_time: Option<SystemTime>,
    /// The current workflow time if it has been established
    current_wf_time: Option<SystemTime>,

    // TODO: Nothing gets deleted from here
    all_machines: SlotMap<MachineKey, Box<dyn TemporalStateMachine + 'static>>,

    /// A mapping for accessing machines associated to a particular event, where the key is the id
    /// of the initiating event for that machine.
    machines_by_event_id: HashMap<i64, MachineKey>,

    // TODO: Nothing gets deleted from here
    /// Maps command ids as created by workflow authors to their associated machines.
    id_to_machine: HashMap<CommandID, MachineKey>,

    /// Queued commands which have been produced by machines and await processing / being sent to
    /// the server.
    commands: VecDeque<CommandAndMachine>,
    /// Commands generated by the currently processing workflow task, which will eventually be
    /// transferred to `commands` (and hence eventually sent to the server)
    ///
    /// Old note: It is a queue as commands can be added (due to marker based commands) while
    /// iterating over already added commands.
    current_wf_task_commands: VecDeque<CommandAndMachine>,
    /// Information about patch markers we have already seen while replaying history
    encountered_change_markers: HashMap<String, ChangeInfo>,

    /// The workflow that is being driven by this instance of the machines
    drive_me: DrivenWorkflow,

    /// Is set to true once we've seen the final event in workflow history, to avoid accidentally
    /// re-applying the final workflow task.
    have_seen_terminal_event: bool,

    /// Metrics context
    pub metrics: MetricsContext,
}

#[derive(Debug, derive_more::Display)]
#[display(fmt = "Cmd&Machine({})", "command")]
struct CommandAndMachine {
    command: ProtoCommand,
    machine: MachineKey,
}

#[derive(Debug, Clone, Copy)]
struct ChangeInfo {
    deprecated: bool,
    created_command: bool,
}

/// Returned by [TemporalStateMachine]s when handling events
#[derive(Debug, derive_more::Display)]
#[must_use]
#[allow(clippy::large_enum_variant)]
pub enum MachineResponse {
    #[display(fmt = "PushWFJob")]
    PushWFJob(wf_activation_job::Variant),

    IssueNewCommand(ProtoCommand),
    #[display(fmt = "TriggerWFTaskStarted")]
    TriggerWFTaskStarted {
        task_started_event_id: i64,
        time: SystemTime,
    },
    #[display(fmt = "UpdateRunIdOnWorkflowReset({})", run_id)]
    UpdateRunIdOnWorkflowReset {
        run_id: String,
    },
}

// Must use `From` b/c ofZZ
impl<T> From<T> for MachineResponse
where
    T: Into<wf_activation_job::Variant>,
{
    fn from(v: T) -> Self {
        MachineResponse::PushWFJob(v.into())
    }
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum WFMachinesError {
    #[error("Nondeterminism error: {0}")]
    Nondeterminism(String),
    #[error("Fatal error in workflow machines: {0}")]
    Fatal(String),

    #[error("Unrecoverable network error while fetching history: {0}")]
    HistoryFetchingError(tonic::Status),

    /// Should always be caught internally and turned into a workflow task failure
    #[error("Unable to process partial event history because workflow is no longer cached.")]
    CacheMiss,
}

impl From<TimestampOutOfSystemRangeError> for WFMachinesError {
    fn from(_: TimestampOutOfSystemRangeError) -> Self {
        WFMachinesError::Fatal("Could not decode timestamp".to_string())
    }
}

impl WorkflowMachines {
    pub(crate) fn new(
        namespace: String,
        workflow_id: String,
        run_id: String,
        history: HistoryUpdate,
        driven_wf: DrivenWorkflow,
        metrics: MetricsContext,
    ) -> Self {
        let replaying = history.previous_started_event_id > 0;
        Self {
            last_history_from_server: history,
            namespace,
            workflow_id,
            run_id,
            drive_me: driven_wf,
            replaying,
            metrics,
            // In an ideal world one could say ..Default::default() here and it'd still work.
            current_started_event_id: 0,
            next_started_event_id: 0,
            workflow_start_time: None,
            workflow_end_time: None,
            current_wf_time: None,
            all_machines: Default::default(),
            machines_by_event_id: Default::default(),
            id_to_machine: Default::default(),
            commands: Default::default(),
            current_wf_task_commands: Default::default(),
            encountered_change_markers: Default::default(),
            have_seen_terminal_event: false,
        }
    }

    /// Returns true if workflow has seen a terminal command
    pub(crate) fn workflow_is_finished(&self) -> bool {
        self.workflow_end_time.is_some()
    }

    /// Returns the total time it took to execute the workflow. Returns `None` if workflow is
    /// incomplete, or time went backwards.
    pub(crate) fn total_runtime(&self) -> Option<Duration> {
        self.workflow_start_time
            .zip(self.workflow_end_time)
            .and_then(|(st, et)| et.duration_since(st).ok())
    }

    pub(crate) async fn new_history_from_server(&mut self, update: HistoryUpdate) -> Result<()> {
        self.last_history_from_server = update;
        self.replaying = self.last_history_from_server.previous_started_event_id > 0;
        self.apply_next_wft_from_history().await?;
        Ok(())
    }

    /// Handle a single event from the workflow history. `has_next_event` should be false if `event`
    /// is the last event in the history.
    ///
    /// TODO: Describe what actually happens in here
    #[instrument(level = "debug", skip(self, event), fields(event=%event))]
    pub(crate) fn handle_event(
        &mut self,
        event: &HistoryEvent,
        has_next_event: bool,
    ) -> Result<()> {
        if event.is_final_wf_execution_event() {
            self.have_seen_terminal_event = true;
        }

        if event.is_command_event() {
            self.handle_command_event(event)?;
            return Ok(());
        }
        if self.replaying
            && self.current_started_event_id
                >= self.last_history_from_server.previous_started_event_id
            && event.event_type() != EventType::WorkflowTaskCompleted
        {
            // Replay is finished
            self.replaying = false;
        }

        match event.get_initial_command_event_id() {
            Some(initial_cmd_id) => {
                // We remove the machine while we it handles events, then return it, to avoid
                // borrowing from ourself mutably.
                let maybe_machine = self.machines_by_event_id.remove(&initial_cmd_id);
                if let Some(sm) = maybe_machine {
                    self.submachine_handle_event(sm, event, has_next_event)?;
                } else {
                    return Err(WFMachinesError::Nondeterminism(format!(
                        "During event handling, this event had an initial command ID but we could \
                        not find a matching command for it: {:?}",
                        event
                    )));
                }

                // Restore machine if not in it's final state
                if let Some(sm) = maybe_machine {
                    if !self.machine(sm).is_final_state() {
                        self.machines_by_event_id.insert(initial_cmd_id, sm);
                    }
                }
            }
            None => self.handle_non_stateful_event(event, has_next_event)?,
        }

        Ok(())
    }

    /// Called when a workflow task started event has triggered. Ensures we are tracking the ID
    /// of the current started event as well as workflow time properly.
    fn task_started(&mut self, task_started_event_id: i64, time: SystemTime) -> Result<()> {
        let s = span!(Level::DEBUG, "Task started trigger");
        let _enter = s.enter();

        // TODO: Local activity machines
        // // Give local activities a chance to recreate their requests if they were lost due
        // // to the last workflow task failure. The loss could happen only the last workflow task
        // // was forcibly created by setting forceCreate on RespondWorkflowTaskCompletedRequest.
        // if (nonProcessedWorkflowTask) {
        //     for (LocalActivityStateMachine value : localActivityMap.values()) {
        //         value.nonReplayWorkflowTaskStarted();
        //     }
        // }

        self.current_started_event_id = task_started_event_id;
        self.set_current_time(time);
        Ok(())
    }

    /// A command event is an event which is generated from a command emitted as a result of
    /// performing a workflow task. Each command has a corresponding event. For example
    /// ScheduleActivityTaskCommand is recorded to the history as ActivityTaskScheduledEvent.
    ///
    /// Command events always follow WorkflowTaskCompletedEvent.
    ///
    /// The handling consists of verifying that the next command in the commands queue is associated
    /// with a state machine, which is then notified about the event and the command is removed from
    /// the commands queue.
    fn handle_command_event(&mut self, event: &HistoryEvent) -> Result<()> {
        // TODO: Local activity handling stuff
        //     if (handleLocalActivityMarker(event)) {
        //       return;
        //     }

        let consumed_cmd = loop {
            if let Some(peek_machine) = self.commands.front() {
                let mach = self.machine(peek_machine.machine);
                match change_marker_handling(event, mach)? {
                    ChangeMarkerOutcome::SkipEvent => return Ok(()),
                    ChangeMarkerOutcome::SkipCommand => {
                        self.commands.pop_front();
                        continue;
                    }
                    ChangeMarkerOutcome::Normal => {}
                }
            }

            let maybe_command = self.commands.pop_front();
            let command = if let Some(c) = maybe_command {
                c
            } else {
                return Err(WFMachinesError::Nondeterminism(format!(
                    "No command scheduled for event {}",
                    event
                )));
            };

            // Feed the machine the event
            let canceled_before_sent = self
                .machine(command.machine)
                .was_cancelled_before_sent_to_server();

            if !canceled_before_sent {
                self.submachine_handle_event(command.machine, event, true)?;
                break command;
            }
        };

        // TODO: validate command

        if !self.machine(consumed_cmd.machine).is_final_state() {
            self.machines_by_event_id
                .insert(event.event_id, consumed_cmd.machine);
        }

        Ok(())
    }

    fn handle_non_stateful_event(
        &mut self,
        event: &HistoryEvent,
        has_next_event: bool,
    ) -> Result<()> {
        debug!(
            event = %event,
            "handling non-stateful event"
        );
        match EventType::from_i32(event.event_type) {
            Some(EventType::WorkflowExecutionStarted) => {
                if let Some(history_event::Attributes::WorkflowExecutionStartedEventAttributes(
                    attrs,
                )) = &event.attributes
                {
                    self.run_id = attrs.original_execution_run_id.clone();
                    if let Some(st) = event.event_time.as_ref() {
                        let as_systime: SystemTime = st.clone().try_into()?;
                        self.workflow_start_time = Some(as_systime);
                    }
                    // We need to notify the lang sdk that it's time to kick off a workflow
                    self.drive_me.send_job(
                        StartWorkflow {
                            workflow_type: attrs
                                .workflow_type
                                .as_ref()
                                .map(|wt| wt.name.clone())
                                .unwrap_or_default(),
                            workflow_id: self.workflow_id.clone(),
                            arguments: Vec::from_payloads(attrs.input.clone()),
                            randomness_seed: str_to_randomness_seed(
                                &attrs.original_execution_run_id,
                            ),
                            headers: match &attrs.header {
                                None => HashMap::new(),
                                Some(Header { fields }) => fields
                                    .iter()
                                    .map(|(k, v)| (k.clone(), Payload::from(v.clone())))
                                    .collect(),
                            },
                        }
                        .into(),
                    );
                    self.drive_me.start(attrs.clone());
                } else {
                    return Err(WFMachinesError::Fatal(format!(
                        "WorkflowExecutionStarted event did not have appropriate attributes: {}",
                        event
                    )));
                }
            }
            Some(EventType::WorkflowTaskScheduled) => {
                let wf_task_sm = WorkflowTaskMachine::new(self.next_started_event_id);
                let key = self.all_machines.insert(Box::new(wf_task_sm));
                self.submachine_handle_event(key, event, has_next_event)?;
                self.machines_by_event_id.insert(event.event_id, key);
            }
            Some(EventType::WorkflowExecutionSignaled) => {
                if let Some(history_event::Attributes::WorkflowExecutionSignaledEventAttributes(
                    attrs,
                )) = &event.attributes
                {
                    self.drive_me.signal(attrs.clone().into());
                } else {
                    // err
                }
            }
            Some(EventType::WorkflowExecutionCancelRequested) => {
                if let Some(
                    history_event::Attributes::WorkflowExecutionCancelRequestedEventAttributes(
                        attrs,
                    ),
                ) = &event.attributes
                {
                    self.drive_me.cancel(attrs.clone().into());
                } else {
                    // err
                }
            }
            _ => {
                return Err(WFMachinesError::Fatal(format!(
                    "The event is non a non-stateful event, but we tried to handle it as one: {}",
                    event
                )));
            }
        }
        Ok(())
    }

    /// Fetches commands which are ready for processing from the state machines, generally to be
    /// sent off to the server. They are not removed from the internal queue, that happens when
    /// corresponding history events from the server are being handled.
    pub(crate) fn get_commands(&self) -> Vec<ProtoCommand> {
        self.commands
            .iter()
            .filter_map(|c| {
                if !self.machine(c.machine).is_final_state() {
                    Some(c.command.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns the next activation that needs to be performed by the lang sdk. Things like unblock
    /// timer, etc. This does *not* cause any advancement of the state machines, it merely drains
    /// from the outgoing queue of activation jobs.
    ///
    /// The job list may be empty, in which case it is expected the caller handles what to do in a
    /// "no work" situation. Possibly, it may know about some work the machines don't, like queries.
    pub(crate) fn get_wf_activation(&mut self) -> WfActivation {
        let jobs = self.drive_me.drain_jobs();
        WfActivation {
            timestamp: self.current_wf_time.map(Into::into),
            is_replaying: self.replaying,
            run_id: self.run_id.clone(),
            jobs,
        }
    }

    fn set_current_time(&mut self, time: SystemTime) -> SystemTime {
        if self.current_wf_time.map(|t| t < time).unwrap_or(true) {
            self.current_wf_time = Some(time);
        }
        self.current_wf_time
            .expect("We have just ensured this is populated")
    }

    /// Iterate the state machines, which consists of grabbing any pending outgoing commands from
    /// the workflow code, handling them, and preparing them to be sent off to the server.
    ///
    /// Returns a boolean flag which indicates whether or not new activations were produced by the
    /// state machine. If true, pending activation should be created by the caller making jobs
    /// available to the lang side.
    pub(crate) async fn iterate_machines(&mut self) -> Result<bool> {
        let results = self.drive_me.fetch_workflow_iteration_output().await;
        let jobs = self.handle_driven_results(results)?;
        let has_new_lang_jobs = !jobs.is_empty();
        for job in jobs.into_iter() {
            self.drive_me.send_job(job);
        }
        self.prepare_commands()?;
        if self.workflow_is_finished() {
            if let Some(rt) = self.total_runtime() {
                self.metrics.wf_e2e_latency(rt);
            }
        }
        Ok(has_new_lang_jobs)
    }

    /// Apply the next (unapplied) entire workflow task from history to these machines. Will replay
    /// any events that need to be replayed until caught up to the newest WFT.
    pub(crate) async fn apply_next_wft_from_history(&mut self) -> Result<()> {
        // A much higher-up span (ex: poll) may want this field filled
        tracing::Span::current().record("run_id", &self.run_id.as_str());

        // If we have already seen the terminal event for the entire workflow in a previous WFT,
        // then we don't need to do anything here, and in fact we need to avoid re-applying the
        // final WFT.
        if self.have_seen_terminal_event {
            return Ok(());
        }

        let last_handled_wft_started_id = self.current_started_event_id;
        let events = self
            .last_history_from_server
            .take_next_wft_sequence(last_handled_wft_started_id)
            .await
            .map_err(WFMachinesError::HistoryFetchingError)?;

        // We're caught up on reply if there are no new events to process
        // TODO: Probably this is unneeded if we evict whenever history is from non-sticky queue
        if events.is_empty() {
            self.replaying = false;
        }
        let replay_start = Instant::now();

        if let Some(last_event) = events.last() {
            if last_event.event_type == EventType::WorkflowTaskStarted as i32 {
                self.next_started_event_id = last_event.event_id;
            }
        }

        let first_event_id = match events.first() {
            Some(event) => event.event_id,
            None => 0,
        };
        // Workflow has been evicted, but we've received partial history from the server.
        // Need to reset sticky and trigger another poll.
        if self.current_started_event_id == 0 && first_event_id != 1 && !events.is_empty() {
            debug!("Cache miss.");
            self.metrics.sticky_cache_miss();
            return Err(WFMachinesError::CacheMiss);
        }

        let mut history = events.iter().peekable();

        while let Some(event) = history.next() {
            let next_event = history.peek();

            if event.event_type == EventType::WorkflowTaskStarted as i32 && next_event.is_none() {
                self.handle_event(event, false)?;
                break;
            }

            self.handle_event(event, next_event.is_some())?;
        }

        // Scan through to the next WFT, searching for any patch markers, so that we can
        // pre-resolve them.
        for e in self.last_history_from_server.peek_next_wft_sequence() {
            if let Some((patch_id, deprecated)) = e.get_changed_marker_details() {
                self.encountered_change_markers.insert(
                    patch_id.clone(),
                    ChangeInfo {
                        deprecated,
                        created_command: false,
                    },
                );
                // Found a patch marker
                self.drive_me
                    .send_job(wf_activation_job::Variant::NotifyHasPatch(NotifyHasPatch {
                        patch_id,
                    }));
            }
        }

        if !self.replaying {
            self.metrics.wf_task_replay_latency(replay_start.elapsed());
        }

        Ok(())
    }

    /// Wrapper for calling [TemporalStateMachine::handle_event] which appropriately takes action
    /// on the returned machine responses
    fn submachine_handle_event(
        &mut self,
        sm: MachineKey,
        event: &HistoryEvent,
        has_next_event: bool,
    ) -> Result<()> {
        let machine_responses = self.machine_mut(sm).handle_event(event, has_next_event)?;
        self.process_machine_responses(sm, machine_responses)?;
        Ok(())
    }

    /// Transfer commands from `current_wf_task_commands` to `commands`, so they may be sent off
    /// to the server. While doing so, [TemporalStateMachine::handle_command] is called on the
    /// machine associated with the command.
    #[instrument(level = "debug", skip(self))]
    fn prepare_commands(&mut self) -> Result<()> {
        while let Some(c) = self.current_wf_task_commands.pop_front() {
            if !self
                .machine(c.machine)
                .was_cancelled_before_sent_to_server()
            {
                let machine_responses = self
                    .machine_mut(c.machine)
                    .handle_command(c.command.command_type())?;
                self.process_machine_responses(c.machine, machine_responses)?;
                self.commands.push_back(c);
            }
        }
        debug!(commands = %self.commands.display(), "prepared commands");
        Ok(())
    }

    /// After a machine handles either an event or a command, it produces [MachineResponses] which
    /// this function uses to drive sending jobs to lang, triggering new workflow tasks, etc.
    fn process_machine_responses(
        &mut self,
        sm: MachineKey,
        machine_responses: Vec<MachineResponse>,
    ) -> Result<()> {
        let sm = self.machine_mut(sm);
        if !machine_responses.is_empty() {
            debug!(responses = %machine_responses.display(), machine_name = %sm.kind(),
                   "Machine produced responses");
        }
        for response in machine_responses {
            match response {
                MachineResponse::PushWFJob(a) => {
                    self.drive_me.send_job(a);
                }
                MachineResponse::TriggerWFTaskStarted {
                    task_started_event_id,
                    time,
                } => {
                    self.task_started(task_started_event_id, time)?;
                }
                MachineResponse::UpdateRunIdOnWorkflowReset { run_id: new_run_id } => {
                    // TODO: Should this also update self.run_id? Should we track orig/current
                    //   separately?
                    self.drive_me
                        .send_job(wf_activation_job::Variant::UpdateRandomSeed(
                            UpdateRandomSeed {
                                randomness_seed: str_to_randomness_seed(&new_run_id),
                            },
                        ));
                }
                MachineResponse::IssueNewCommand(_) => {
                    panic!("Issue new command machine response not expected here")
                }
            }
        }
        Ok(())
    }

    /// Handles results of the workflow activation, delegating work to the appropriate state
    /// machine. Returns a list of workflow jobs that should be queued in the pending activation for
    /// the next poll. This list will be populated only if state machine produced lang activations
    /// as part of command processing. For example some types of activity cancellation need to
    /// immediately unblock lang side without having it to poll for an actual workflow task from the
    /// server.
    fn handle_driven_results(
        &mut self,
        results: Vec<WFCommand>,
    ) -> Result<Vec<wf_activation_job::Variant>> {
        let mut jobs = vec![];
        for cmd in results {
            match cmd {
                WFCommand::AddTimer(attrs) => {
                    let seq = attrs.seq;
                    let timer = self.add_new_command_machine(new_timer(attrs));
                    self.id_to_machine
                        .insert(CommandID::Timer(seq), timer.machine);
                    self.current_wf_task_commands.push_back(timer);
                }
                WFCommand::CancelTimer(attrs) => {
                    jobs.extend(self.process_cancellation(CommandID::Timer(attrs.seq))?)
                }
                WFCommand::AddActivity(attrs) => {
                    let seq = attrs.seq;
                    let activity = self.add_new_command_machine(new_activity(attrs));
                    self.id_to_machine
                        .insert(CommandID::Activity(seq), activity.machine);
                    self.current_wf_task_commands.push_back(activity);
                }
                WFCommand::RequestCancelActivity(attrs) => {
                    jobs.extend(self.process_cancellation(CommandID::Activity(attrs.seq))?)
                }
                WFCommand::CompleteWorkflow(attrs) => {
                    self.metrics.wf_completed();
                    self.add_terminal_command(complete_workflow(attrs));
                }
                WFCommand::FailWorkflow(attrs) => {
                    self.metrics.wf_failed();
                    self.add_terminal_command(fail_workflow(attrs));
                }
                WFCommand::ContinueAsNew(attrs) => {
                    self.metrics.wf_continued_as_new();
                    self.add_terminal_command(continue_as_new(attrs));
                }
                WFCommand::CancelWorkflow(attrs) => {
                    self.metrics.wf_canceled();
                    self.add_terminal_command(cancel_workflow(attrs));
                }
                WFCommand::SetPatchMarker(attrs) => {
                    // Do not create commands for change IDs that we have already created commands
                    // for.
                    if !matches!(self.encountered_change_markers.get(&attrs.patch_id),
                                Some(ChangeInfo {created_command, ..})
                                    if *created_command)
                    {
                        let verm = self.add_new_command_machine(has_change(
                            attrs.patch_id.clone(),
                            self.replaying,
                            attrs.deprecated,
                        ));
                        self.current_wf_task_commands.push_back(verm);

                        if let Some(ci) = self.encountered_change_markers.get_mut(&attrs.patch_id) {
                            ci.created_command = true;
                        } else {
                            self.encountered_change_markers.insert(
                                attrs.patch_id,
                                ChangeInfo {
                                    deprecated: attrs.deprecated,
                                    created_command: true,
                                },
                            );
                        }
                    }
                }
                WFCommand::AddChildWorkflow(attrs) => {
                    let seq = attrs.seq;
                    let child_workflow = self.add_new_command_machine(new_child_workflow(attrs));
                    self.id_to_machine
                        .insert(CommandID::ChildWorkflowStart(seq), child_workflow.machine);
                    self.current_wf_task_commands.push_back(child_workflow);
                }
                WFCommand::CancelUnstartedChild(attrs) => jobs.extend(self.process_cancellation(
                    CommandID::ChildWorkflowStart(attrs.child_workflow_seq),
                )?),
                WFCommand::RequestCancelExternalWorkflow(attrs) => {
                    let (we, only_child) = match attrs.target {
                        None => {
                            return Err(WFMachinesError::Fatal(
                                "Cancel external workflow command had empty target field"
                                    .to_string(),
                            ))
                        }
                        Some(cancel_we::Target::ChildWorkflowId(wfid)) => (
                            NamespacedWorkflowExecution {
                                namespace: self.namespace.clone(),
                                workflow_id: wfid,
                                run_id: "".to_string(),
                            },
                            true,
                        ),
                        Some(cancel_we::Target::WorkflowExecution(we)) => (we, false),
                    };
                    let mach = self
                        .add_new_command_machine(new_external_cancel(attrs.seq, we, only_child));
                    self.id_to_machine
                        .insert(CommandID::CancelExternal(attrs.seq), mach.machine);
                    self.current_wf_task_commands.push_back(mach);
                }
                WFCommand::SignalExternalWorkflow(attrs) => {
                    let (we, only_child) = match attrs.target {
                        None => {
                            return Err(WFMachinesError::Fatal(
                                "Signal external workflow command had empty target field"
                                    .to_string(),
                            ))
                        }
                        Some(sig_we::Target::ChildWorkflowId(wfid)) => (
                            NamespacedWorkflowExecution {
                                namespace: self.namespace.clone(),
                                workflow_id: wfid,
                                run_id: "".to_string(),
                            },
                            true,
                        ),
                        Some(sig_we::Target::WorkflowExecution(we)) => (we, false),
                    };

                    let sigm = self.add_new_command_machine(new_external_signal(
                        attrs.seq,
                        we,
                        attrs.signal_name,
                        attrs.args,
                        only_child,
                    ));
                    self.id_to_machine
                        .insert(CommandID::SignalExternal(attrs.seq), sigm.machine);
                    self.current_wf_task_commands.push_back(sigm);
                }
                WFCommand::CancelSignalWorkflow(attrs) => {
                    jobs.extend(self.process_cancellation(CommandID::SignalExternal(attrs.seq))?)
                }
                WFCommand::QueryResponse(_) => {
                    // Nothing to do here, queries are handled above the machine level
                    unimplemented!("Query responses should not make it down into the machines")
                }
                WFCommand::NoCommandsFromLang => (),
            }
        }
        Ok(jobs)
    }

    /// Given a command id to attempt to cancel, try to cancel it and return any jobs that should
    /// be included in the activation
    fn process_cancellation(&mut self, id: CommandID) -> Result<Vec<Variant>> {
        let mut jobs = vec![];
        let m_key = self.get_machine_key(id)?;
        let res = self.machine_mut(m_key).cancel()?;
        debug!(machine_responses = ?res, cmd_id = ?id, "Cancel request responses");
        for r in res {
            match r {
                MachineResponse::IssueNewCommand(c) => {
                    self.current_wf_task_commands.push_back(CommandAndMachine {
                        command: c,
                        machine: m_key,
                    })
                }
                MachineResponse::PushWFJob(j) => {
                    jobs.push(j);
                }
                v => {
                    return Err(WFMachinesError::Fatal(format!(
                        "Unexpected machine response {:?} when cancelling {:?}",
                        v, id
                    )));
                }
            }
        }
        Ok(jobs)
    }

    fn get_machine_key(&self, id: CommandID) -> Result<MachineKey> {
        Ok(*self.id_to_machine.get(&id).ok_or_else(|| {
            WFMachinesError::Fatal(format!("Missing associated machine for {:?}", id))
        })?)
    }

    fn add_terminal_command<T: TemporalStateMachine + 'static>(
        &mut self,
        machine: NewMachineWithCommand<T>,
    ) {
        let cwfm = self.add_new_command_machine(machine);
        self.workflow_end_time = Some(SystemTime::now());
        self.current_wf_task_commands.push_back(cwfm);
    }

    fn add_new_command_machine<T: TemporalStateMachine + 'static>(
        &mut self,
        machine: NewMachineWithCommand<T>,
    ) -> CommandAndMachine {
        let k = self.all_machines.insert(Box::new(machine.machine));
        CommandAndMachine {
            command: machine.command,
            machine: k,
        }
    }

    fn machine(&self, m: MachineKey) -> &dyn TemporalStateMachine {
        self.all_machines
            .get(m)
            .expect("Machine must exist")
            .borrow()
    }

    fn machine_mut(&mut self, m: MachineKey) -> &mut (dyn TemporalStateMachine + 'static) {
        self.all_machines
            .get_mut(m)
            .expect("Machine must exist")
            .borrow_mut()
    }
}

fn str_to_randomness_seed(run_id: &str) -> u64 {
    let mut s = DefaultHasher::new();
    run_id.hash(&mut s);
    s.finish()
}

enum ChangeMarkerOutcome {
    SkipEvent,
    SkipCommand,
    Normal,
}

/// Special handling for patch markers, when handling command events as in
/// [WorkflowMachines::handle_command_event]
fn change_marker_handling(
    event: &HistoryEvent,
    mach: &dyn TemporalStateMachine,
) -> Result<ChangeMarkerOutcome> {
    if !mach.matches_event(event) {
        // Version markers can be skipped in the event they are deprecated
        if let Some(changed_info) = event.get_changed_marker_details() {
            // Is deprecated. We can simply ignore this event, as deprecated change
            // markers are allowed without matching changed calls.
            if changed_info.1 {
                debug!("Deprecated patch marker tried against wrong machine, skipping.");
                return Ok(ChangeMarkerOutcome::SkipEvent);
            }
            return Err(WFMachinesError::Nondeterminism(format!(
                "Non-deprecated patch marker encountered for change {}, \
                            but there is no corresponding change command!",
                changed_info.0
            )));
        }
        // Version machines themselves may also not *have* matching markers, where non-deprecated
        // calls take the old path, and deprecated calls assume history is produced by a new-code
        // worker.
        if mach.kind() == MachineKind::Version {
            debug!("Skipping non-matching event against version machine");
            return Ok(ChangeMarkerOutcome::SkipCommand);
        }
    }
    Ok(ChangeMarkerOutcome::Normal)
}
