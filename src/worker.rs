use std::collections::HashMap;

use futures::future;
use futures::future::FutureExt;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use rmp_serde as rmps;
use tokio::codec::Framed;
use tokio::net::TcpStream;

use crate::common::WrappedRcRefCell;
use crate::core::Core;
use crate::daskcodec::{DaskCodec, DaskMessage};
use crate::messages::aframe::AfDescriptor;
use crate::messages::generic::RegisterWorkerMsg;
use crate::messages::workermsg::{DeleteDataMsg, FromWorkerMessage, GetDataMsg, HeartbeatResponse, Status, ToWorkerMessage};
use crate::prelude::*;
use crate::task::{ErrorInfo, TaskRuntimeState};

pub struct Worker {
    pub id: WorkerId,
    pub sender: tokio::sync::mpsc::UnboundedSender<Bytes>,
    pub ncpus: u32,
    pub listen_address: String,
}

#[derive(Default)]
pub struct WorkerUpdate {
    pub compute_tasks: Vec<TaskRef>,
    pub delete_keys: Vec<TaskKey>,
}

pub type WorkerUpdateMap = HashMap<WorkerRef, WorkerUpdate>;

impl Worker {
    #[inline]
    pub fn id(&self) -> WorkerId {
        self.id
    }

    pub fn make_sched_info(&self) -> crate::scheduler::schedproto::WorkerInfo {
        crate::scheduler::schedproto::WorkerInfo {
            id: self.id,
            ncpus: self.ncpus,
        }
    }

    pub fn send_message(&mut self, message: Vec<ToWorkerMessage>) -> crate::Result<()> {
        let data = rmp_serde::encode::to_vec_named(&message).unwrap();
        self.sender.try_send(data.into()).unwrap(); // TODO: bail!("Send of worker XYZ failed")
        Ok(())
    }
}

pub type WorkerRef = WrappedRcRefCell<Worker>;

pub async fn start_worker(
    core_ref: &CoreRef,
    address: std::net::SocketAddr,
    framed: Framed<TcpStream, DaskCodec>,
    msg: RegisterWorkerMsg,
) -> crate::Result<()> {
    let core_ref = core_ref.clone();
    let core_ref2 = core_ref.clone();

    let (mut snd_sender, mut snd_receiver) = tokio::sync::mpsc::unbounded_channel::<Bytes>();

    let hb = HeartbeatResponse {
        status: "OK",
        time: 0.0,
        heartbeat_interval: 1.0,
        worker_plugins: Vec::new(),
    };
    let data = rmp_serde::encode::to_vec_named(&hb)?;
    snd_sender.try_send(data.into()).unwrap();

    let (worker_id, worker_ref) = {
        let mut core = core_ref.get_mut();
        let worker_id = core.new_worker_id();
        let worker_ref = WorkerRef::wrap(Worker {
            id: worker_id,
            ncpus: 1, // TODO: real cpus
            sender: snd_sender,
            listen_address: msg.address,
        });
        core.register_worker(worker_ref.clone());
        (worker_id, worker_ref)
    };

    log::info!("New worker registered as {} from {}", worker_id, address);

    let (mut sender, receiver) = framed.split();
    let snd_loop = async move {
        while let Some(data) = snd_receiver.next().await {
            if let Err(e) = sender.send(data.into()).await {
                log::error!("Send to worker failed");
                return Err(e);
            }
        }
        Ok(())
    }
        .boxed_local();

    let recv_loop = receiver.try_for_each(move |mut data| {
        let msgs: Result<Vec<FromWorkerMessage>, _> = rmps::from_read(std::io::Cursor::new(&data.message));
        if let Err(e) = msgs {
            dbg!(data);
            panic!("Invalid message from worker ({}): {}", worker_id, e);
        }

        let mut aframe_map: HashMap<u64, _> = if !data.additional_frames.is_empty() {
            let descriptor: AfDescriptor = rmps::from_slice(&data.additional_frames.remove(0)).unwrap();
            descriptor.split_frames_by_index(data.additional_frames).unwrap()
        } else {
            Default::default()
        };

        //let frame_map = crate::aframe::parse_additional_frames(data.additional_frames);

        /*let mut new_ready_scheduled = Vec::new();
        let mut task_to_delete = HashMap::new();*/

        let mut worker_updates = HashMap::new();

        for (i, msg) in msgs.unwrap().into_iter().enumerate() {
            match msg {
                FromWorkerMessage::TaskFinished(msg) => {
                    assert!(msg.status == Status::Ok); // TODO: handle other cases ??
                    let mut core = core_ref.get_mut();
                    core.on_task_finished(&worker_ref, msg, &mut worker_updates);
                }
                FromWorkerMessage::TaskErred(msg) => {
                    assert!(msg.status == Status::Error); // TODO: handle other cases ??
                    let error_info = ErrorInfo {
                        frames: aframe_map.remove(&(i as u64)).unwrap_or_else(Vec::new),
                    };
                    let mut core = core_ref.get_mut();
                    core.on_task_error(&worker_ref, msg.key, error_info);
                }
                FromWorkerMessage::KeepAlive => { /* Do nothing by design */ }
            }
        }

        let core = core_ref.get();
        send_worker_updates(&core, worker_updates);

        /*if !new_ready_scheduled.is_empty() {
            let mut tasks_per_worker: HashMap<WorkerRef, Vec<TaskRef>> = HashMap::new();
            for task_ref in new_ready_scheduled {
                let worker = {
                    let mut task = task_ref.get_mut();
                    let worker_ref = task.worker.clone().unwrap();
                    task.state = TaskRuntimeState::Assigned;
                    log::debug!("Task id={} assigned to worker={}", task.id, worker_ref.get().id);
                    worker_ref
                };
                let v = tasks_per_worker.entry(worker).or_insert_with(Vec::new);
                v.push(task_ref);
            }
            let core = core_ref.get();
            send_tasks_to_workers(&core, tasks_per_worker);
        }*/
        future::ready(Ok(()))
    });

    let result = future::select(recv_loop, snd_loop).await;
    if let Err(e) = result.factor_first().0 {
        log::error!(
            "Error in worker connection (id={}, connection={}): {}",
            worker_id,
            address,
            e
        );
    }
    log::info!(
        "Worker {} connection closed (connection: {})",
        worker_id,
        address
    );
    let mut core = core_ref2.get_mut();
    core.unregister_worker(worker_id);
    Ok(())
}


pub fn send_worker_updates(core: &Core, worker_updates: WorkerUpdateMap) {
    for (worker_ref, w_update) in worker_updates {
        let mut msgs: Vec<_> = w_update.compute_tasks.iter().map(|t| ToWorkerMessage::ComputeTask(t.get().make_compute_task_msg(core))).collect();
        if !w_update.delete_keys.is_empty() {
            msgs.push(ToWorkerMessage::DeleteData(DeleteDataMsg { keys: w_update.delete_keys, report: false }));
        }
        if !msgs.is_empty() {
            let mut worker = worker_ref.get_mut();
            worker.send_message(msgs).unwrap_or_else(|_| {
                // !!! Do not propagate error right now, we need to finish sending messages to others
                // Worker cleanup is done elsewhere (when worker future terminates),
                // so we can safely ignore this. Since we are nice guys we log (debug) message.
                log::debug!("Sending tasks to worker {} failed", worker.id);
            });
        }
    }
}


pub async fn get_data_from_worker<'b>(worker: &WorkerRef, keys: &'b Vec<&str>) -> crate::Result<DaskMessage>
{
    let connection = {
        let worker = worker.get();
        let address = &worker.listen_address.trim_start_matches("tcp://");
        TcpStream::connect(address).await?
    };
    let mut connection = Framed::new(connection, DaskCodec::new());

    // send get data request
    let msg = ToWorkerMessage::GetData(GetDataMsg {
        keys,
        who: None,
        max_connections: false,
        reply: true,
    });
    connection.send(rmp_serde::to_vec_named(&msg)?.into()).await?;

    // TODO: Error propagation
    // TODO: Storing worker connection?
    Ok(connection.next().await.unwrap()?)
}