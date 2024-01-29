#![deny(warnings)]

wasmtime::component::bindgen!({
    path: "../wit",
    interfaces: "
        use isyswasfa:isyswasfa/isyswasfa.{poll-input, poll-output};

        import isyswasfa:isyswasfa/isyswasfa;
        import isyswasfa:io/poll;

        export dummy: func(input: poll-input) -> poll-output;                
    ",
    async: {
        only_imports: []
    },
    with: {
        "wasi:io/poll/pollable": Pollable,
        "isyswasfa:isyswasfa/isyswasfa/ready": Task,
        "isyswasfa:isyswasfa/isyswasfa/pending": Task,
        "isyswasfa:isyswasfa/isyswasfa/cancel": Task,
    }
});

use {
    anyhow::{anyhow, bail},
    futures::{
        channel::oneshot,
        future::{self, Either, FutureExt, Select},
        stream::{FuturesUnordered, ReadyChunks, StreamExt},
    },
    isyswasfa::{
        io::poll::Host as PollHost,
        isyswasfa::isyswasfa::{
            Host as IsyswasfaHost, HostCancel, HostPending, HostReady, PollInput, PollInputCancel,
            PollInputListening, PollInputReady, PollOutput, PollOutputListen, PollOutputPending,
            PollOutputReady,
        },
    },
    once_cell::sync::Lazy,
    std::{
        any::Any,
        collections::{HashMap, HashSet},
        future::Future,
        pin::Pin,
        sync::Arc,
        task::{Context, Poll, Wake, Waker},
    },
    wasmparser::{ComponentExternalKind, Parser, Payload},
    wasmtime::{
        component::{Instance, Linker, Resource, ResourceTable, TypedFunc},
        StoreContextMut,
    },
    wasmtime_wasi::preview2::{MakeFuture, WasiView},
};

pub use {isyswasfa::isyswasfa::isyswasfa as interface, wasmtime_wasi::preview2::Pollable};

pub fn add_to_linker<T: WasiView + IsyswasfaView + Send>(
    linker: &mut Linker<T>,
) -> wasmtime::Result<()> {
    isyswasfa::isyswasfa::isyswasfa::add_to_linker(linker, |ctx| ctx)?;
    isyswasfa::io::poll::add_to_linker(linker, |ctx| ctx)
}

fn dummy_waker() -> Waker {
    struct DummyWaker;

    impl Wake for DummyWaker {
        fn wake(self: Arc<Self>) {}
    }

    static WAKER: Lazy<Arc<DummyWaker>> = Lazy::new(|| Arc::new(DummyWaker));

    WAKER.clone().into()
}

#[derive(Copy, Clone)]
struct StatePoll {
    state: u32,
    poll: usize,
}

type BoxFuture = Pin<Box<dyn Future<Output = (u32, Box<dyn Any + Send>)> + Send + 'static>>;

pub struct Task {
    reference_count: u32,
    state: TaskState,
    listen: Option<StatePoll>,
}

pub struct GuestPending {
    on_cancel: Option<StatePoll>,
}

pub enum TaskState {
    FuturePending(Option<oneshot::Sender<()>>),
    FutureReady(Option<Box<dyn Any + Send>>),
    GuestPending(GuestPending),
    GuestReady(u32),
    PollablePending {
        stream: u32,
        make_future: MakeFuture,
    },
    PollableReady,
}

type PollFunc = TypedFunc<(Vec<PollInput>,), (Vec<PollOutput>,)>;

pub struct IsyswasfaCtx {
    table: ResourceTable,
    futures: ReadyChunks<FuturesUnordered<Select<oneshot::Receiver<()>, BoxFuture>>>,
    pollables: HashSet<u32>,
    polls: Vec<PollFunc>,
}

impl Default for IsyswasfaCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl IsyswasfaCtx {
    pub fn new() -> Self {
        Self {
            table: ResourceTable::new(),
            futures: FuturesUnordered::new().ready_chunks(1024),
            pollables: HashSet::new(),
            polls: Vec::new(),
        }
    }

    pub fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    fn guest_pending(
        &mut self,
    ) -> wasmtime::Result<(Resource<Task>, Resource<Task>, Resource<Task>)> {
        let pending = self.table.push(Task {
            reference_count: 3,
            state: TaskState::GuestPending(GuestPending { on_cancel: None }),
            listen: None,
        })?;
        let cancel = Resource::new_own(pending.rep());
        let ready = Resource::new_own(pending.rep());

        Ok((pending, cancel, ready))
    }

    pub fn first_poll<T: Send + 'static>(
        &mut self,
        future: impl Future<Output = wasmtime::Result<T>> + Send + 'static,
    ) -> wasmtime::Result<Result<T, Resource<Task>>> {
        let (tx, rx) = oneshot::channel();
        let task = self.table.push(Task {
            reference_count: 1,
            state: TaskState::FuturePending(Some(tx)),
            listen: None,
        })?;
        let rep = task.rep();
        let mut future =
            Box::pin(future.map(move |v| (rep, Box::new(v) as Box<dyn Any + Send>))) as BoxFuture;

        Ok(
            match future
                .as_mut()
                .poll(&mut Context::from_waker(&dummy_waker()))
            {
                Poll::Ready((_, result)) => {
                    self.drop(task)?;
                    Ok((*result.downcast::<wasmtime::Result<T>>().unwrap())?)
                }
                Poll::Pending => {
                    self.futures.get_mut().push(future::select(rx, future));
                    Err(task)
                }
            },
        )
    }

    fn guest_state(&self, ready: Resource<Task>) -> wasmtime::Result<u32> {
        match &self.table.get(&ready)?.state {
            TaskState::GuestReady(guest_state) => Ok(*guest_state),
            _ => Err(anyhow!("unexpected task state")),
        }
    }

    fn drop(&mut self, handle: Resource<Task>) -> wasmtime::Result<()> {
        let task = self.table.get_mut(&handle)?;
        task.reference_count = task.reference_count.checked_sub(1).unwrap();
        if task.reference_count == 0 {
            self.table.delete(handle)?;
        }
        Ok(())
    }

    pub fn get_ready<T: 'static>(&mut self, ready: Resource<Task>) -> wasmtime::Result<T> {
        let value = match &mut self.table.get_mut(&ready)?.state {
            TaskState::FutureReady(value) => *value.take().unwrap().downcast().unwrap(),
            _ => bail!("unexpected task state"),
        };

        self.drop(ready)?;

        value
    }

    async fn wait(&mut self, input: &mut HashMap<usize, Vec<PollInput>>) -> wasmtime::Result<()> {
        tracing::trace!(
            "wait for {} pollables and {} futures",
            self.pollables.len(),
            self.futures.get_ref().len()
        );

        let ready = if self.pollables.is_empty() {
            if self.futures.get_ref().is_empty() {
                bail!("guest task is pending with no pending host tasks");
            } else {
                Either::Right(self.futures.next().await)
            }
        } else {
            let pollables = self
                .pollables
                .iter()
                .map(|&index| {
                    if let Task {
                        state:
                            TaskState::PollablePending {
                                stream,
                                make_future,
                            },
                        ..
                    } = self.table.get(&Resource::<Task>::new_own(index))?
                    {
                        Ok((*stream, (*make_future, index)))
                    } else {
                        unreachable!()
                    }
                })
                .collect::<wasmtime::Result<HashMap<_, _>>>()?;

            let mut futures = self
                .table
                .iter_entries(pollables)
                .map(|(entry, (make_future, index))| Ok((make_future(entry?), index)))
                .collect::<wasmtime::Result<Vec<_>>>()?;

            let ready = future::poll_fn(move |cx| {
                let mut any_ready = false;
                let mut results = Vec::new();
                for (fut, index) in futures.iter_mut() {
                    match fut.as_mut().poll(cx) {
                        Poll::Ready(()) => {
                            results.push(*index);
                            any_ready = true;
                        }
                        Poll::Pending => {}
                    }
                }
                if any_ready {
                    Poll::Ready(results)
                } else {
                    Poll::Pending
                }
            });

            if self.futures.get_ref().is_empty() {
                Either::Left(ready.await)
            } else {
                match future::select(ready, self.futures.next()).await {
                    Either::Left((pollables, _)) => Either::Left(pollables),
                    Either::Right((values, _)) => Either::Right(values),
                }
            }
        };

        match ready {
            Either::Left(pollables) => {
                for index in pollables {
                    let ready = Resource::<Task>::new_own(index);
                    let task = self.table.get_mut(&ready)?;
                    task.reference_count += 1;
                    task.state = TaskState::PollableReady;
                    self.pollables.remove(&index);

                    if let Some(listen) = task.listen {
                        input.entry(listen.poll).or_default().push(PollInput::Ready(
                            PollInputReady {
                                state: listen.state,
                                ready,
                            },
                        ));
                    }
                }
            }
            Either::Right(values) => {
                if let Some(values) = values {
                    for value in values {
                        match value {
                            Either::Left(_) => {}
                            Either::Right(((task_rep, result), _)) => {
                                let ready = Resource::<Task>::new_own(task_rep);
                                let task = self.table.get_mut(&ready)?;
                                task.reference_count += 1;
                                task.state = TaskState::FutureReady(Some(result));

                                if let Some(listen) = task.listen {
                                    input.entry(listen.poll).or_default().push(PollInput::Ready(
                                        PollInputReady {
                                            state: listen.state,
                                            ready,
                                        },
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

fn task<'a, T: IsyswasfaView + Send>(
    store: &'a mut StoreContextMut<'a, T>,
    handle: &Resource<Task>,
) -> wasmtime::Result<&'a mut Task> {
    Ok(store.data_mut().isyswasfa().table().get_mut(handle)?)
}

fn state<'a, T: IsyswasfaView + Send>(
    store: &'a mut StoreContextMut<'a, T>,
    handle: &Resource<Task>,
) -> wasmtime::Result<&'a mut TaskState> {
    Ok(&mut task(store, handle)?.state)
}

fn guest_pending<'a, T: IsyswasfaView + Send>(
    store: &'a mut StoreContextMut<'a, T>,
    handle: &Resource<Task>,
) -> wasmtime::Result<&'a mut GuestPending> {
    Ok::<_, wasmtime::Error>(match state(store, handle)? {
        TaskState::GuestPending(pending) => pending,
        _ => unreachable!(),
    })
}

fn isyswasfa<'a, T: IsyswasfaView + Send>(
    store: &'a mut StoreContextMut<'a, T>,
) -> &'a mut IsyswasfaCtx {
    store.data_mut().isyswasfa()
}

fn drop<'a, T: IsyswasfaView + Send>(
    store: &'a mut StoreContextMut<'a, T>,
    handle: Resource<Task>,
) -> wasmtime::Result<()> {
    isyswasfa(store).drop(handle)
}

pub fn load_poll_funcs<S: wasmtime::AsContextMut>(
    mut store: S,
    component: &[u8],
    instance: &Instance,
) -> wasmtime::Result<()>
where
    <S as wasmtime::AsContext>::Data: IsyswasfaView + Send,
{
    let mut names = Vec::new();
    for payload in Parser::new(0).parse_all(component) {
        if let Payload::ComponentExportSection(reader) = payload? {
            for export in reader {
                let export = export?;
                if let ComponentExternalKind::Func = export.kind {
                    if export.name.0.starts_with("isyswasfa-poll") {
                        names.push(export.name.0);
                    }
                }
            }
        }
    }

    if names.is_empty() {
        bail!("unable to find any function exports with names starting with `isyswasfa-poll`");
    }

    let polls = {
        let mut store = store.as_context_mut();
        let mut exports = instance.exports(&mut store);
        let mut exports = exports.root();
        names
            .into_iter()
            .map(|name| exports.typed_func::<(Vec<PollInput>,), (Vec<PollOutput>,)>(name))
            .collect::<wasmtime::Result<_>>()?
    };

    store.as_context_mut().data_mut().isyswasfa().polls = polls;

    Ok(())
}

pub async fn await_ready<S: wasmtime::AsContextMut>(
    mut store: S,
    pending: Resource<Task>,
) -> wasmtime::Result<Resource<Task>>
where
    <S as wasmtime::AsContext>::Data: IsyswasfaView + Send,
{
    let mut count = 0;
    let polls = isyswasfa(&mut store.as_context_mut()).polls.clone();

    let mut result = None;
    let mut input = HashMap::new();
    loop {
        count += 1;
        if count > 20 {
            panic!();
        }

        for (index, poll) in polls.iter().enumerate() {
            tracing::trace!("input: {:?}", input.get(&index));

            let output = poll
                .call_async(
                    store.as_context_mut(),
                    (input.remove(&index).unwrap_or_else(Vec::new),),
                )
                .await?
                .0;
            poll.post_return_async(store.as_context_mut()).await?;

            tracing::trace!("output: {output:?}");

            for output in output {
                match output {
                    PollOutput::Ready(PollOutputReady {
                        state: guest_state,
                        ready,
                    }) => {
                        if ready.rep() == pending.rep() {
                            *state(&mut store.as_context_mut(), &ready)? =
                                TaskState::GuestReady(guest_state);

                            result = Some(ready);
                        } else if let Some(listen) =
                            task(&mut store.as_context_mut(), &ready)?.listen
                        {
                            input.entry(listen.poll).or_default().push(PollInput::Ready(
                                PollInputReady {
                                    state: listen.state,
                                    ready,
                                },
                            ));
                        } else {
                            let tmp = Resource::new_own(ready.rep());
                            *state(&mut store.as_context_mut(), &tmp)? =
                                TaskState::GuestReady(guest_state);
                        }
                    }
                    PollOutput::Listen(PollOutputListen { state, pending }) => {
                        let context = &mut store.as_context_mut();
                        let task = task(context, &pending)?;
                        task.listen = Some(StatePoll { state, poll: index });

                        match task.state {
                            TaskState::FuturePending(_) | TaskState::PollablePending { .. } => {
                                input.entry(index).or_default().push(PollInput::Listening(
                                    PollInputListening {
                                        state,
                                        cancel: pending,
                                    },
                                ))
                            }
                            TaskState::FutureReady(_) | TaskState::PollableReady => input
                                .entry(index)
                                .or_default()
                                .push(PollInput::Ready(PollInputReady {
                                    state,
                                    ready: pending,
                                })),
                            TaskState::GuestPending(GuestPending { on_cancel: Some(_) }) => {
                                input.entry(index).or_default().push(PollInput::Listening(
                                    PollInputListening {
                                        state,
                                        cancel: pending,
                                    },
                                ));
                            }
                            TaskState::GuestPending(_) => {
                                drop(&mut store.as_context_mut(), pending)?;
                            }
                            TaskState::GuestReady(_) => {
                                input.entry(index).or_default().push(PollInput::Ready(
                                    PollInputReady {
                                        state,
                                        ready: pending,
                                    },
                                ));
                            }
                        }
                    }
                    PollOutput::Pending(PollOutputPending { state, cancel }) => {
                        if cancel.rep() == pending.rep() {
                            drop(&mut store.as_context_mut(), cancel)?;
                        } else {
                            guest_pending(&mut store.as_context_mut(), &cancel)?.on_cancel =
                                Some(StatePoll { state, poll: index });

                            if let Some(listen) = task(&mut store.as_context_mut(), &cancel)?.listen
                            {
                                input
                                    .entry(listen.poll)
                                    .or_default()
                                    .push(PollInput::Listening(PollInputListening {
                                        state: listen.state,
                                        cancel,
                                    }));
                            }
                        }
                    }
                    PollOutput::Cancel(cancel) => {
                        let context = &mut store.as_context_mut();
                        let task = task(context, &cancel)?;
                        let listen = task.listen.unwrap();

                        let mut cancel_host_task = |store, cancel| {
                            drop(store, cancel)?;
                            input
                                .entry(listen.poll)
                                .or_default()
                                .push(PollInput::CancelComplete(listen.state));
                            Ok::<_, wasmtime::Error>(())
                        };

                        match &mut task.state {
                            TaskState::FuturePending(cancel_tx) => {
                                cancel_tx.take();
                                cancel_host_task(&mut store.as_context_mut(), cancel)?;
                            }
                            TaskState::PollablePending { .. }
                            | TaskState::FutureReady(_)
                            | TaskState::PollableReady => {
                                cancel_host_task(&mut store.as_context_mut(), cancel)?;
                            }
                            TaskState::GuestPending(GuestPending { on_cancel, .. }) => {
                                let on_cancel = on_cancel.unwrap();

                                input
                                    .entry(on_cancel.poll)
                                    .or_default()
                                    .push(PollInput::Cancel(PollInputCancel {
                                        state: listen.state,
                                        cancel,
                                    }));
                            }
                            TaskState::GuestReady(_) => unreachable!(),
                        }
                    }
                    PollOutput::CancelComplete(cancel) => {
                        let listen = task(&mut store.as_context_mut(), &cancel)?.listen.unwrap();

                        input
                            .entry(listen.poll)
                            .or_default()
                            .push(PollInput::CancelComplete(listen.state));
                    }
                }
            }
        }

        if input.is_empty() {
            if let Some(ready) = result.take() {
                drop(&mut store.as_context_mut(), pending)?;

                break Ok(ready);
            } else {
                store
                    .as_context_mut()
                    .data_mut()
                    .isyswasfa()
                    .wait(&mut input)
                    .await?;
            }
        }
    }
}

pub trait IsyswasfaView {
    type State: 'static;

    fn isyswasfa(&mut self) -> &mut IsyswasfaCtx;
    fn state(&self) -> Self::State;
}

impl<T: IsyswasfaView> HostPending for T {
    fn drop(&mut self, this: Resource<Task>) -> wasmtime::Result<()> {
        self.isyswasfa().drop(this)
    }
}

impl<T: IsyswasfaView> HostCancel for T {
    fn drop(&mut self, this: Resource<Task>) -> wasmtime::Result<()> {
        self.isyswasfa().drop(this)
    }
}

impl<T: IsyswasfaView> HostReady for T {
    fn state(&mut self, this: Resource<Task>) -> wasmtime::Result<u32> {
        self.isyswasfa().guest_state(this)
    }

    fn drop(&mut self, this: Resource<Task>) -> wasmtime::Result<()> {
        self.isyswasfa().drop(this)
    }
}

impl<T: IsyswasfaView> IsyswasfaHost for T {
    fn make_task(&mut self) -> wasmtime::Result<(Resource<Task>, Resource<Task>, Resource<Task>)> {
        self.isyswasfa().guest_pending()
    }
}

impl<T: IsyswasfaView> PollHost for T {
    fn block_isyswasfa(
        &mut self,
        this: Resource<Pollable>,
    ) -> wasmtime::Result<Result<(), Resource<Task>>> {
        let cx = self.isyswasfa();
        let pollable = cx.table.get(&this)?;
        let index = pollable.index;
        let make_future = pollable.make_future;
        {
            let mut ready = (make_future)(cx.table.get_any_mut(index)?);
            match ready
                .as_mut()
                .poll(&mut Context::from_waker(&dummy_waker()))
            {
                Poll::Ready(_) => return Ok(Ok(())),
                Poll::Pending => {}
            }
        }

        let task = cx.table.push_child(
            Task {
                reference_count: 1,
                state: TaskState::PollablePending {
                    stream: index,
                    make_future,
                },
                listen: None,
            },
            &Resource::<()>::new_own(index),
        )?;
        cx.pollables.insert(task.rep());
        Ok(Err(task))
    }

    fn block_isyswasfa_result(&mut self, ready: Resource<Task>) -> wasmtime::Result<()> {
        let cx = self.isyswasfa();
        if let TaskState::PollableReady = cx.table.get(&ready)?.state {
            cx.drop(ready)?;

            Ok(())
        } else {
            Err(anyhow!("unexpected task state"))
        }
    }
}
