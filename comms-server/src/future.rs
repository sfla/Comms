use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use futures::channel::oneshot;

use comms_lib::serialization::IncomingPacketFrame;

pub struct SendResponse {
    sender: oneshot::Sender<IncomingPacketFrame>,
    waker: Rc<RefCell<Option<Waker>>>,
}

impl SendResponse {
    pub fn done(self, response: IncomingPacketFrame) {
        let _ = self.sender.send(response); // Timeouts can cause the result of a future to become irrelevant
        let mut waker = self.waker.borrow_mut();
        if let Some(waker) = waker.take() {
            waker.wake()
        }
    }
}

pub struct NetworkFuture {
    receiver: Rc<RefCell<oneshot::Receiver<IncomingPacketFrame>>>,
    waker: Rc<RefCell<Option<Waker>>>,
}

impl Future for NetworkFuture {
    type Output = IncomingPacketFrame;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut receiver = self.receiver.borrow_mut();
        match receiver.try_recv() {
            Ok(Some(response)) => Poll::Ready(response),
            Ok(None) => {
                let mut waker = self.waker.borrow_mut();
                *waker = Some(cx.waker().clone());
                Poll::Pending
            }
            Err(_) => {
                panic!("NetworkFuture was cancelled (other side didn't send response?)");
            }
        }
    }
}
