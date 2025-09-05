use std::cell::RefCell;
use std::rc::Rc;
use std::task::Waker;

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
