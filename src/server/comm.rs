use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::common::{Map, WrappedRcRefCell};
use crate::Error;
use crate::scheduler::{FromSchedulerMessage, ToSchedulerMessage};
use crate::server::client::{Client, ClientId};
use crate::server::core::{Core, CoreRef};
use crate::server::notifications::{ClientNotification, WorkerNotification};
use crate::server::notifications::Notifications;
use crate::server::protocol::daskmessages::client::{KeyInMemoryMsg, TaskErredMsg, ToClientMessage};
use crate::server::protocol::dasktransport::DaskPacket;
use crate::server::protocol::dasktransport::MessageBuilder;
use crate::server::task::TaskRuntimeState;
use crate::server::worker::WorkerRef;
use crate::trace::trace_task_send;

pub type CommRef = WrappedRcRefCell<Comm>;

pub struct Comm {
    sender: UnboundedSender<Vec<ToSchedulerMessage>>,
}

impl Comm {
    pub fn notify(&mut self, core: &mut Core, notifications: Notifications) -> crate::Result<()> {
        if !notifications.scheduler_messages.is_empty() {
            self.sender.send(notifications.scheduler_messages).unwrap();
        }
        self.notify_workers(&core, notifications.workers)?;
        self.notify_clients(core, notifications.clients)?;

        Ok(())
    }

    fn notify_clients(
        &mut self,
        core: &mut Core,
        notifications: Map<ClientId, ClientNotification>,
    ) -> crate::Result<()> {
        for (client_id, c_update) in notifications {
            let mut mbuilder = MessageBuilder::<ToClientMessage>::with_capacity(
                c_update.error_tasks.len() + c_update.in_memory_tasks.len(),
            );
            for task_ref in c_update.error_tasks {
                let task = task_ref.get();
                if let TaskRuntimeState::Error(error_info) = &task.state {
                    let exception = mbuilder.copy_serialized(&error_info.exception);
                    let traceback = mbuilder.copy_serialized(&error_info.traceback);
                    mbuilder.add_message(ToClientMessage::TaskErred(TaskErredMsg {
                        key: task.key().into(),
                        exception,
                        traceback,
                    }));
                } else {
                    panic!("Task is not in error state");
                };
            }

            for task_ref in c_update.in_memory_tasks {
                let task = task_ref.get();
                mbuilder.add_message(ToClientMessage::KeyInMemory(KeyInMemoryMsg {
                    key: task.key().into(),
                    r#type: task.data_info().unwrap().r#type.clone(),
                }));
            }

            if !mbuilder.is_empty() {
                self.send_client_packet(
                    core.get_client_by_id_or_panic(client_id),
                    mbuilder.build_batch()?,
                )?;
            }
        }
        Ok(())
    }

    fn notify_workers(
        &mut self,
        core: &Core,
        notifications: Map<WorkerRef, WorkerNotification>,
    ) -> crate::Result<()> {
        for (worker_ref, w_update) in notifications {
            let worker = worker_ref.get();
            for task in w_update.compute_tasks {
                let task = task.get();
                trace_task_send(task.id, worker_ref.get().id);
                task.make_compute_task_msg(core, &worker);
            }

            /*let mut mbuilder = MessageBuilder::default();

            for task in w_update.compute_tasks {
                let task = task.get();
                trace_task_send(task.id, worker_ref.get().id);
                task.make_compute_task_msg(core, &mut mbuilder);
            }

            if !w_update.delete_keys.is_empty() {
                mbuilder.add_message(ToWorkerMessage::DeleteData(DeleteDataMsg {
                    keys: w_update.delete_keys,
                    report: false,
                }));
            }

            for tref in w_update.steal_tasks {
                let task = tref.get();
                mbuilder.add_message(ToWorkerMessage::StealRequest(StealRequestMsg {
                    key: task.key().into(),
                }));
            }

            if !mbuilder.is_empty() {
                self.send_worker_message(&worker_ref, mbuilder.build_batch()?)
                    .unwrap_or_else(|_| {
                        // !!! Do not propagate error right now, we need to finish sending protocol to others
                        // Worker cleanup is done elsewhere (when worker future terminates),
                        // so we can safely ignore this. Since we are nice guys we log (debug) message.
                        log::debug!("Sending tasks to worker {} failed", worker_ref.get().id);
                    });
            }*/
        }

        Ok(())
    }

    /*#[inline]
    fn send_worker_message(&mut self, worker: &WorkerRef, packet: Bytes) -> crate::Result<()> {
        worker.get_mut().send_message(packet)
    }*/

    #[inline]
    fn send_client_packet(&mut self, client: &mut Client, packet: DaskPacket) -> crate::Result<()> {
        client.send_dask_packet(packet)
    }
}

impl CommRef {
    pub fn new(sender: UnboundedSender<Vec<ToSchedulerMessage>>) -> Self {
        Self::wrap(Comm { sender })
    }
}

pub async fn observe_scheduler(
    core_ref: CoreRef,
    comm_ref: CommRef,
    mut receiver: UnboundedReceiver<FromSchedulerMessage>,
) -> crate::Result<()> {
    log::debug!("Starting scheduler");

    match receiver.next().await {
        Some(crate::scheduler::FromSchedulerMessage::Register(r)) => {
            log::debug!("Scheduler registered: {:?}", r)
        }
        None => {
            return Err(Error::SchedulerError(
                "Scheduler closed connection without registration".to_owned(),
            ));
        }
        _ => {
            return Err(Error::SchedulerError(
                "First message of scheduler has to be registration".to_owned(),
            ));
        }
    }

    while let Some(msg) = receiver.next().await {
        match msg {
            FromSchedulerMessage::TaskAssignments(assignments) => {
                let mut core = core_ref.get_mut();
                let mut notifications = Default::default();

                trace_time!("core", "process_assignments", {
                    core.process_assignments(assignments, &mut notifications);
                    trace_time!("core", "notify", {
                        comm_ref.get_mut().notify(&mut core, notifications).unwrap();
                    });
                });
            }
            FromSchedulerMessage::Register(_) => {
                return Err(Error::SchedulerError(
                    "Double registration of scheduler".to_owned(),
                ));
            }
        }
    }

    Ok(())
}
