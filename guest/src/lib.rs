#![deny(warnings)]

mod bindings {
    wit_bindgen::generate!({
        path: "../wit",
        inline: "
            package foo:foo;
            world foo {
                use isyswasfa:isyswasfa/isyswasfa.{poll-input, poll-output};

                import isyswasfa:isyswasfa/isyswasfa;
                import isyswasfa:io/poll;
                import wasi:io/streams@0.2.0;

                export dummy: func(input: poll-input) -> poll-output;
            }
        ",
        exports: {
            world: World
        }
    });

    struct World;

    impl Guest for World {
        fn dummy(_input: PollInput) -> PollOutput {
            unreachable!()
        }
    }
}

use {
    bindings::{
        isyswasfa::{
            io::poll,
            isyswasfa::isyswasfa::{
                self, Cancel, Pending, PollInput, PollInputCancel, PollInputListening,
                PollInputReady, PollOutput, PollOutputListen, PollOutputPending, PollOutputReady,
                Ready,
            },
        },
        wasi::io::{
            poll::Pollable,
            streams::{InputStream, OutputStream, StreamError},
        },
    },
    by_address::ByAddress,
    futures::{channel::oneshot, future::FutureExt},
    once_cell::sync::Lazy,
    std::{
        any::Any,
        cell::RefCell,
        collections::HashMap,
        future::Future,
        future::IntoFuture,
        mem,
        ops::{Deref, DerefMut},
        pin::Pin,
        rc::Rc,
        sync::Arc,
        task::{Context, Poll, Wake},
    },
};

pub use bindings::{
    isyswasfa::isyswasfa::isyswasfa as isyswasfa_interface,
    wasi::io::{poll as poll_interface, streams as streams_interface},
};

struct FutureStateWaker(Rc<RefCell<FutureState>>);

unsafe impl Send for FutureStateWaker {}
unsafe impl Sync for FutureStateWaker {}

impl Wake for FutureStateWaker {
    fn wake(self: Arc<Self>) {
        insert_pollable(self.0.clone())
    }
}

type BoxFuture = Pin<Box<dyn Future<Output = Box<dyn Any>> + 'static>>;

enum CancelState {
    Pending,
    Cancel,
    Listening(Cancel),
}

struct CancelOnDrop(Rc<RefCell<CancelState>>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        match mem::replace(self.0.borrow_mut().deref_mut(), CancelState::Cancel) {
            CancelState::Pending | CancelState::Cancel => {}
            CancelState::Listening(cancel) => push_output(PollOutput::Cancel(cancel)),
        }
    }
}

struct PendingState {
    pending: Pending,
    tx: oneshot::Sender<Ready>,
    cancel_state: Rc<RefCell<CancelState>>,
}

type Pollables = HashMap<ByAddress<Rc<RefCell<FutureState>>>, Rc<RefCell<FutureState>>>;

static mut POLLABLES: Lazy<Pollables> = Lazy::new(HashMap::new);

fn insert_pollable(pollable: Rc<RefCell<FutureState>>) {
    unsafe { POLLABLES.insert(ByAddress(pollable.clone()), pollable) };
}

fn take_pollables() -> Pollables {
    unsafe { mem::take(POLLABLES.deref_mut()) }
}

static mut POLL_OUTPUT: Vec<PollOutput> = Vec::new();

fn push_output(output: PollOutput) {
    unsafe { POLL_OUTPUT.push(output) }
}

static mut PENDING: Vec<PendingState> = Vec::new();

fn add_pending(pending_state: PendingState) {
    unsafe { PENDING.push(pending_state) }
}

fn set_pending(pending: Vec<PendingState>) {
    unsafe {
        PENDING = pending;
    }
}

fn take_pending_nonempty() -> Vec<PendingState> {
    let pending = take_pending();
    assert!(!pending.is_empty());
    pending
}

fn take_pending() -> Vec<PendingState> {
    unsafe { mem::take(&mut PENDING) }
}

fn clear_pending() {
    unsafe { PENDING.clear() }
}

struct ListenState {
    tx: oneshot::Sender<Ready>,
    future_state: Rc<RefCell<FutureState>>,
    cancel_state: Rc<RefCell<CancelState>>,
}

enum FutureState {
    Pending {
        ready: Option<Ready>,
        future: BoxFuture,
        cancel_states: Vec<Rc<RefCell<CancelState>>>,
    },
    Cancelled(Option<Cancel>),
    Ready(Option<Box<dyn Any>>),
}

impl Drop for FutureState {
    fn drop(&mut self) {
        match self {
            Self::Pending { .. } => (),
            Self::Cancelled(cancel) => {
                push_output(PollOutput::CancelComplete(cancel.take().unwrap()))
            }
            Self::Ready(ready) => assert!(ready.is_none()),
        }
    }
}

fn push_listens(future_state: &Rc<RefCell<FutureState>>) {
    for pending in take_pending_nonempty() {
        push_output(PollOutput::Listen(PollOutputListen {
            pending: pending.pending,
            state: u32::try_from(Box::into_raw(Box::new(ListenState {
                tx: pending.tx,
                cancel_state: pending.cancel_state,
                future_state: future_state.clone(),
            })) as usize)
            .unwrap(),
        }));
    }
}

pub fn first_poll<T: 'static>(future: impl Future<Output = T> + 'static) -> Result<T, Pending> {
    let mut future = Box::pin(future.map(|v| Box::new(v) as Box<dyn Any>)) as BoxFuture;
    let future_state = Rc::new(RefCell::new(FutureState::Ready(None)));

    match future.as_mut().poll(&mut Context::from_waker(
        &Arc::new(FutureStateWaker(future_state.clone())).into(),
    )) {
        Poll::Pending => {
            let (pending, cancel, ready) = isyswasfa::make_task();
            *future_state.borrow_mut() = FutureState::Pending {
                ready: Some(ready),
                future,
                cancel_states: Vec::new(),
            };

            push_listens(&future_state);

            push_output(PollOutput::Pending(PollOutputPending {
                cancel,
                state: u32::try_from(Rc::into_raw(future_state) as usize).unwrap(),
            }));

            Err(pending)
        }
        Poll::Ready(result) => {
            clear_pending();
            Ok(*result.downcast().unwrap())
        }
    }
}

pub fn spawn(future: impl Future<Output = ()> + 'static) {
    let pending = take_pending();
    drop(first_poll(future));
    set_pending(pending);
}

pub fn get_ready<T: 'static>(ready: Ready) -> T {
    match unsafe { Rc::from_raw(ready.state() as usize as *const RefCell<FutureState>) }
        .borrow_mut()
        .deref_mut()
    {
        FutureState::Ready(value) => *value.take().unwrap().downcast().unwrap(),
        _ => unreachable!(),
    }
}

fn cancel_all(cancels: &[Rc<RefCell<CancelState>>]) {
    for cancel in cancels {
        match mem::replace(cancel.borrow_mut().deref_mut(), CancelState::Cancel) {
            CancelState::Pending | CancelState::Cancel => {}
            CancelState::Listening(cancel) => push_output(PollOutput::Cancel(cancel)),
        }
    }
}

pub fn poll(input: Vec<PollInput>) -> Vec<PollOutput> {
    for input in input {
        match input {
            PollInput::Listening(PollInputListening { state, cancel }) => {
                let listen_state =
                    unsafe { (state as usize as *const ListenState).as_ref().unwrap() };

                let listening = match listen_state.cancel_state.borrow().deref() {
                    CancelState::Pending => true,
                    CancelState::Cancel => false,
                    CancelState::Listening(_) => unreachable!(),
                };

                if listening {
                    match listen_state.future_state.borrow_mut().deref_mut() {
                        FutureState::Pending { cancel_states, .. } => {
                            cancel_states.push(listen_state.cancel_state.clone())
                        }
                        _ => unreachable!(),
                    }

                    *listen_state.cancel_state.borrow_mut() = CancelState::Listening(cancel)
                } else {
                    push_output(PollOutput::Cancel(cancel));
                }
            }
            PollInput::Ready(PollInputReady { state, ready }) => {
                let listen_state = *unsafe { Box::from_raw(state as usize as *mut ListenState) };

                match mem::replace(
                    listen_state.cancel_state.borrow_mut().deref_mut(),
                    CancelState::Cancel,
                ) {
                    CancelState::Pending | CancelState::Listening(_) => {
                        drop(listen_state.tx.send(ready))
                    }
                    CancelState::Cancel => {}
                }

                insert_pollable(listen_state.future_state);
            }
            PollInput::Cancel(PollInputCancel { state, cancel }) => {
                let future_state =
                    unsafe { Rc::from_raw(state as usize as *const RefCell<FutureState>) };

                let mut old = mem::replace(
                    future_state.borrow_mut().deref_mut(),
                    FutureState::Cancelled(Some(cancel)),
                );

                match &mut old {
                    FutureState::Pending { cancel_states, .. } => cancel_all(cancel_states),
                    FutureState::Cancelled(_) => unreachable!(),
                    FutureState::Ready(ready) => drop(ready.take()),
                }
            }
            PollInput::CancelComplete(state) => unsafe {
                drop(Box::from_raw(state as usize as *mut ListenState))
            },
        }
    }

    loop {
        let pollables = take_pollables();

        if pollables.is_empty() {
            break unsafe { mem::take(&mut POLL_OUTPUT) };
        } else {
            for future_state in pollables.into_values() {
                let poll = match future_state.borrow_mut().deref_mut() {
                    FutureState::Pending { future, .. } => {
                        future.as_mut().poll(&mut Context::from_waker(
                            &Arc::new(FutureStateWaker(future_state.clone())).into(),
                        ))
                    }
                    _ => continue,
                };

                match poll {
                    Poll::Pending => push_listens(&future_state),
                    Poll::Ready(result) => {
                        clear_pending();

                        let mut old = mem::replace(
                            future_state.borrow_mut().deref_mut(),
                            FutureState::Ready(Some(result)),
                        );

                        let FutureState::Pending {
                            ready,
                            cancel_states,
                            ..
                        } = &mut old
                        else {
                            unreachable!()
                        };

                        cancel_all(cancel_states);

                        push_output(PollOutput::Ready(PollOutputReady {
                            ready: ready.take().unwrap(),
                            state: u32::try_from(Rc::into_raw(future_state) as usize).unwrap(),
                        }));
                    }
                }
            }
        }
    }
}

pub async fn await_ready(pending: Pending) -> Ready {
    let (tx, rx) = oneshot::channel();
    let cancel_state = Rc::new(RefCell::new(CancelState::Pending));
    add_pending(PendingState {
        pending,
        tx,
        cancel_state: cancel_state.clone(),
    });
    let _cancel_on_drop = CancelOnDrop(cancel_state);
    rx.await.unwrap()
}

impl IntoFuture for Pollable {
    type Output = ();
    // TODO: use a custom future here to avoid the overhead of boxing
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'static>>;

    fn into_future(self) -> Self::IntoFuture {
        let v = poll::block_isyswasfa(&self);
        Box::pin(async move {
            let _self = self;
            match v {
                Ok(()) => (),
                Err(pending) => poll::block_isyswasfa_result(await_ready(pending).await),
            }
        })
    }
}

pub async fn copy(rx: &InputStream, tx: &OutputStream) -> Result<(), StreamError> {
    // TODO: use `OutputStream::splice`
    while let Some(chunk) = read(rx, 64 * 1024).await? {
        write_all(tx, &chunk).await?;
    }
    Ok(())
}

pub async fn write_all(tx: &OutputStream, chunk: &[u8]) -> Result<(), StreamError> {
    let mut offset = 0;
    while offset < chunk.len() {
        let count = usize::try_from(tx.check_write()?)
            .unwrap()
            .min(chunk.len() - offset);

        if count > 0 {
            tx.write(&chunk[offset..][..count])?;
            offset += count
        } else {
            tx.subscribe().await
        }
    }
    Ok(())
}

pub async fn read(rx: &InputStream, max: u64) -> Result<Option<Vec<u8>>, StreamError> {
    loop {
        match rx.read(max) {
            Ok(chunk) if chunk.is_empty() => rx.subscribe().await,
            Ok(chunk) => break Ok(Some(chunk)),
            Err(StreamError::Closed) => break Ok(None),
            Err(error) => break Err(error),
        }
    }
}
