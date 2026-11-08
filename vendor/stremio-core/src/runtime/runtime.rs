use crate::runtime::msg::{Action, Event, Msg};
use crate::runtime::{Effect, EffectFuture, Env, Model};
use derivative::Derivative;
use enclose::enclose;
use futures::FutureExt;
#[cfg(test)]
use futures::SinkExt;
use futures::channel::mpsc::{Receiver, Sender, channel};
use serde::Serialize;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::time::{Duration, Instant};

/// How long a thread may wait for the model lock before we log a report. The
/// report names every thread that currently holds the lock, which is the only
/// way to identify a lock that is never released: `/proc` only tells us that
/// the waiters are sleeping in `futex`, not who owns the lock.
const SLOW_LOCK: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
struct LockHolder {
    thread_id: std::thread::ThreadId,
    thread_name: String,
    since: Instant,
    count: usize,
    location: &'static std::panic::Location<'static>,
}

static READ_HOLDERS: OnceLock<Mutex<Vec<LockHolder>>> = OnceLock::new();
static WRITE_HOLDER: OnceLock<Mutex<Option<LockHolder>>> = OnceLock::new();

fn read_holders() -> &'static Mutex<Vec<LockHolder>> {
    READ_HOLDERS.get_or_init(|| Mutex::new(Vec::new()))
}

fn write_holder() -> &'static Mutex<Option<LockHolder>> {
    WRITE_HOLDER.get_or_init(|| Mutex::new(None))
}

fn current_thread() -> (std::thread::ThreadId, String) {
    let thread = std::thread::current();
    (thread.id(), thread.name().unwrap_or("<unnamed>").to_owned())
}

fn register_reader(location: &'static std::panic::Location<'static>) {
    let (thread_id, thread_name) = current_thread();
    let mut holders = read_holders()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(holder) = holders
        .iter_mut()
        .find(|holder| holder.thread_id == thread_id)
    {
        holder.count += 1;
        holder.location = location;
        return;
    }
    holders.push(LockHolder {
        thread_id,
        thread_name,
        since: Instant::now(),
        location,
        count: 1,
    });
}

fn unregister_reader() {
    let thread_id = std::thread::current().id();
    let mut holders = read_holders()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(position) = holders
        .iter()
        .position(|holder| holder.thread_id == thread_id)
    {
        if holders[position].count > 1 {
            holders[position].count -= 1;
        } else {
            holders.remove(position);
        }
    }
}

fn register_writer(location: &'static std::panic::Location<'static>) {
    let (thread_id, thread_name) = current_thread();
    *write_holder()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(LockHolder {
        thread_id,
        thread_name,
        since: Instant::now(),
        count: 1,
        location,
    });
}

fn unregister_writer() {
    *write_holder()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// Logs who owns the model lock and what every thread of the process is doing.
/// Called from the thread that is waiting, so the waiter's own identity is
/// known even when the lock owner is a thread that never returns.
fn report_contention(kind: &str, waited: Duration) {
    let (waiter_id, waiter_name) = current_thread();
    let readers = read_holders()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let writer = write_holder()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let reentrant = readers.iter().any(|holder| holder.thread_id == waiter_id);
    tracing::error!(
        lock = kind,
        waited_ms = waited.as_millis(),
        waiter = %waiter_name,
        reentrant_read = reentrant,
        readers = ?readers
            .iter()
            .map(|holder| format!(
                "{} ({} read(s), {:.1}s) at {}",
                holder.thread_name,
                holder.count,
                holder.since.elapsed().as_secs_f64(),
                holder.location
            ))
            .collect::<Vec<_>>(),
        writer = ?writer.as_ref().map(|holder| format!(
            "{} ({:.1}s) at {}",
            holder.thread_name,
            holder.since.elapsed().as_secs_f64(),
            holder.location
        )),
        "model lock waited too long; dumping thread states"
    );
    #[cfg(target_os = "linux")]
    {
        let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
            return;
        };
        for task in tasks.flatten() {
            let path = task.path();
            let read = |name: &str| {
                std::fs::read_to_string(path.join(name))
                    .map(|value| value.trim().to_owned())
                    .unwrap_or_default()
            };
            let state = read("status")
                .lines()
                .find(|line| line.starts_with("State:"))
                .unwrap_or("")
                .to_owned();
            tracing::error!(
                tid = %task.file_name().to_string_lossy(),
                name = %read("comm"),
                %state,
                wchan = %read("wchan"),
                "thread state while waiting for the model lock"
            );
        }
    }
}

fn acquire_read<'a, M>(
    lock: &'a RwLock<M>,
    kind: &str,
    location: &'static std::panic::Location<'static>,
) -> Result<ModelReadGuard<'a, M>, String> {
    let started = Instant::now();
    loop {
        match lock.try_read() {
            Ok(guard) => {
                register_reader(location);
                return Ok(ModelReadGuard { guard });
            }
            Err(TryLockError::Poisoned(error)) => {
                return Err(format!("model lock poisoned: {error}"));
            }
            Err(TryLockError::WouldBlock) => {
                if started.elapsed() >= SLOW_LOCK {
                    report_contention(kind, started.elapsed());
                    // Keep the original blocking behaviour: the report above
                    // documents the deadlock, it does not try to break it.
                    let guard = lock
                        .read()
                        .map_err(|error| format!("model lock poisoned: {error}"))?;
                    register_reader(location);
                    return Ok(ModelReadGuard { guard });
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

fn acquire_write<'a, M>(
    lock: &'a RwLock<M>,
    location: &'static std::panic::Location<'static>,
) -> Result<ModelWriteGuard<'a, M>, String> {
    let started = Instant::now();
    loop {
        match lock.try_write() {
            Ok(guard) => {
                register_writer(location);
                return Ok(ModelWriteGuard { guard });
            }
            Err(TryLockError::Poisoned(error)) => {
                return Err(format!("model lock poisoned: {error}"));
            }
            Err(TryLockError::WouldBlock) => {
                if started.elapsed() >= SLOW_LOCK {
                    report_contention("write", started.elapsed());
                    let guard = lock
                        .write()
                        .map_err(|error| format!("model lock poisoned: {error}"))?;
                    register_writer(location);
                    return Ok(ModelWriteGuard { guard });
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// Read guard that unregisters its thread from the lock holder registry on
/// drop, so a slow lock acquisition can name the owner.
pub struct ModelReadGuard<'a, M> {
    guard: RwLockReadGuard<'a, M>,
}

impl<'a, M> Deref for ModelReadGuard<'a, M> {
    type Target = M;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a, M> Drop for ModelReadGuard<'a, M> {
    fn drop(&mut self) {
        unregister_reader();
    }
}

struct ModelWriteGuard<'a, M> {
    guard: RwLockWriteGuard<'a, M>,
}

impl<'a, M> Deref for ModelWriteGuard<'a, M> {
    type Target = M;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a, M> DerefMut for ModelWriteGuard<'a, M> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl<'a, M> Drop for ModelWriteGuard<'a, M> {
    fn drop(&mut self) {
        unregister_writer();
    }
}

#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(tag = "name", content = "args")]
pub enum RuntimeEvent<E: Env, M: Model<E>> {
    NewState(Vec<M::Field>, #[cfg(test)] M),
    CoreEvent(Event),
}

#[derive(Debug)]
pub struct RuntimeAction<E: Env, M: Model<E>> {
    pub field: Option<M::Field>,
    pub action: Action,
}

#[derive(Derivative)]
#[derivative(Clone(bound = ""))]
pub struct Runtime<E: Env, M: Model<E>> {
    model: Arc<RwLock<M>>,
    tx: Sender<RuntimeEvent<E, M>>,
    env: PhantomData<E>,
}

impl<E, M> Runtime<E, M>
where
    E: Env + Send + 'static,
    M: Model<E> + Send + Sync + 'static,
{
    pub fn new(
        model: M,
        effects: Vec<Effect>,
        buffer: usize,
    ) -> (Self, Receiver<RuntimeEvent<E, M>>) {
        let (tx, rx) = channel(buffer);
        let model = Arc::new(RwLock::new(model));
        let runtime = Runtime {
            model,
            tx,
            env: PhantomData,
        };
        runtime.handle_effects(effects, vec![]);
        (runtime, rx)
    }
    #[track_caller]
    pub fn model(&self) -> Result<ModelReadGuard<'_, M>, String> {
        acquire_read(&self.model, "read", std::panic::Location::caller())
    }
    #[track_caller]
    fn write_model(&self) -> ModelWriteGuard<'_, M> {
        acquire_write(&self.model, std::panic::Location::caller()).expect("model write failed")
    }
    #[track_caller]
    pub fn dispatch(&self, action: RuntimeAction<E, M>) {
        let (effects, fields) = {
            let mut model = self.write_model();
            match action {
                RuntimeAction {
                    field: Some(field),
                    action,
                } => model.update_field(&Msg::Action(action), &field),
                RuntimeAction { action, .. } => model.update(&Msg::Action(action)),
            }
        };
        self.handle_effects(effects, fields);
    }
    #[cfg(test)]
    pub async fn close(&mut self) -> Result<(), anyhow::Error> {
        self.tx.flush().await?;
        self.tx.close_channel();
        Ok(())
    }
    fn emit(&self, event: RuntimeEvent<E, M>) {
        self.tx.clone().try_send(event).expect("emit event failed");
    }
    fn handle_effects(&self, effects: Vec<Effect>, fields: Vec<M::Field>) {
        if !fields.is_empty() {
            #[cfg(test)]
            let model = self.model().expect("model read failed");
            self.emit(RuntimeEvent::<E, M>::NewState(
                fields,
                #[cfg(test)]
                model.to_owned(),
            ));
        };
        effects
            .into_iter()
            .for_each(enclose!((self.clone() => runtime) move |effect| {
                match effect {
                    Effect::Msg(msg) => {
                        runtime.handle_effect_output(*msg);
                    }
                    Effect::Future(EffectFuture::Sequential(future)) => {
                        E::exec_sequential(future.then(enclose!((runtime) move |msg| async move {
                            runtime.handle_effect_output(msg);
                        })))
                    },
                    Effect::Future(EffectFuture::Concurrent(future)) => {
                        E::exec_concurrent(future.then(enclose!((runtime) move |msg| async move {
                            runtime.handle_effect_output(msg);
                        })))
                    }
                }
            }));
    }
    fn handle_effect_output(&self, msg: Msg) {
        match msg {
            Msg::Event(event) => {
                self.emit(RuntimeEvent::CoreEvent(event));
            }
            Msg::Internal(_) => {
                let (effects, fields) = self.write_model().update(&msg);
                self.handle_effects(effects, fields);
            }
            Msg::Action(_) => {
                panic!("effects are not allowed to resolve with action");
            }
        }
    }
}
