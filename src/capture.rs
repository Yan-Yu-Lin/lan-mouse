use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};
use input_event::{Event, KeyboardEvent, scancode};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::connect::LanMouseConnection;

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
}

pub(crate) enum ICaptureEvent {
    /// a client was entered
    CaptureBegin(CaptureHandle),
    /// capture disabled
    CaptureDisabled,
    /// capture disabled
    CaptureEnabled,
    /// The active client was left (capture released for any reason).
    ClientLeft(CaptureHandle),
    ClientReady(CaptureHandle),
    ClientFailed,
    /// A (new) client was entered.
    /// In contrast to [`ICaptureEvent::CaptureBegin`] this
    /// event is only triggered when the capture was
    /// explicitly released in the meantime by
    /// either the remote client leaving its device region,
    /// a new device entering the screen or the release bind.
    ClientEntered(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Clone, Debug)]
enum CaptureRequest {
    /// capture must release the mouse
    Release(bool),
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
    /// enter the given client without a barrier crossing
    Enter(CaptureHandle),
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: LanMouseConnection,
        release_bind: Vec<scancode::Linux>,
        hotkey_only: bool,
        switch_hook: Option<String>,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let capture_task = CaptureTask {
            active_client: None,
            hotkey_only,
            switch_hook,
            serial: 0,
            next_serial: 0,
            started: None,
            buffered: Default::default(),
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            state: Default::default(),
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            request_tx,
            task,
            event_rx,
        }
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: lan_mouse_ipc::Position,
        capture_type: CaptureType,
    ) {
        let pos = to_capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type))
            .expect("channel closed");
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .expect("channel closed");
    }

    pub(crate) fn release(&self) {
        self.request_tx
            .send(CaptureRequest::Release(false))
            .expect("channel closed");
    }

    pub(crate) fn release_hotkey(&self) {
        self.request_tx
            .send(CaptureRequest::Release(true))
            .expect("channel closed");
    }

    pub(crate) fn enter(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Enter(handle))
            .expect("channel closed");
    }

    pub(crate) async fn event(&mut self) -> ICaptureEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }
}

struct CaptureTask {
    switch_hook: Option<String>,
    hotkey_only: bool,
    serial: u32,
    next_serial: u32,
    started: Option<Instant>,
    buffered: std::collections::VecDeque<Event>,
    active_client: Option<CaptureHandle>,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<(CaptureHandle, Position, CaptureType)>,
    conn: LanMouseConnection,
    event_tx: Sender<ICaptureEvent>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
}

impl CaptureTask {
    fn add_capture(&mut self, handle: CaptureHandle, pos: Position, capture_type: CaptureType) {
        self.captures.push((handle, pos, capture_type));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t)| p == pos && t == CaptureType::Default)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Position {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .1
    }

    fn get_type(&self, handle: CaptureHandle) -> CaptureType {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .2
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_capture().await {
                log::warn!("input capture exited: {e}");
            }
            loop {
                tokio::select! {
                    r = self.request_rx.recv() => match r.expect("channel closed") {
                        CaptureRequest::Reenable => break,
                        CaptureRequest::Create(h, p, t) => self.add_capture(h, p, t),
                        CaptureRequest::Destroy(h) => self.remove_capture(h),
                        CaptureRequest::Release(_) => { crate::switching::run_hook(self.switch_hook.clone(), "error").await;
                        self.event_tx.send(ICaptureEvent::ClientFailed).expect("channel closed"); }
                        CaptureRequest::Enter(_) => { crate::switching::run_hook(self.switch_hook.clone(), "error").await;
                        self.event_tx.send(ICaptureEvent::ClientFailed).expect("channel closed"); }
                        CaptureRequest::SetReleaseBind(bind) => {
                            self.release_bind.borrow_mut().clone_from(&bind);
                        }
                    },
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };

        capture.set_hotkey_only(self.hotkey_only).await;

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers for active clients */
        let r = self.create_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        let r = self.do_capture_session(&mut capture).await;

        // the backend is going away: if a client was still active, the
        // logical capture state must not survive into the next backend
        if let Some(handle) = self.active_client.take() {
            log::warn!("capture session ended while client {handle} was active");
            self.state = State::default();
            self.event_tx
                .send(ICaptureEvent::ClientLeft(handle))
                .expect("channel closed");
        }

        self.started = None;
        self.buffered.clear();
        self.serial = 0;
        crate::switching::run_hook(
            self.switch_hook.clone(),
            if r.is_err() { "error" } else { "reset" },
        )
        .await;

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let captures = self.captures.clone();
        for (handle, pos, _type) in captures {
            if self.hotkey_only && _type == CaptureType::EnterOnly {
                continue;
            }
            tokio::select! {
                r = capture.create(handle, pos) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        let mut retry = tokio::time::interval(Duration::from_millis(75));
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = retry.tick(), if self.started.is_some() && self.state == State::WaitingForAck => {
                    if self.started.is_some_and(|t| t.elapsed() >= Duration::from_secs(3)) {
                        log::warn!("switch timed out after 3000ms");
                        self.release_capture(capture, false).await?;
                        crate::switching::run_hook(self.switch_hook.clone(), "error").await;
                        self.event_tx.send(ICaptureEvent::ClientFailed).expect("channel closed");
                    } else if let Some(handle) = self.active_client {
                        let _ = self.conn.send(self.enter_event(handle), handle).await;
                    }
                },
                event = capture.next() => match event {
                    Some(event) => self.handle_capture_event(capture, event?).await?,
                    None => return Ok(()),
                },
                (handle, event) = self.conn.recv() => {
                    if let Some(active) = self.active_client {
                        if handle != active {
                            // we only care about events coming from the client we are currently connected to
                            // only `Ack` and `Leave` are relevant
                            continue
                        }
                    }

                    match event {
                        // connection acknowlegded => set state to Sending
                        ProtoEvent::Ack(serial) => {
                            if self.active_client == Some(handle) && self.state == State::WaitingForAck && serial == self.serial {
                                self.state = State::Sending;
                                if let Some(started) = self.started.take() {
                                    log::info!("switch ready: client={handle} elapsed_ms={}", started.elapsed().as_millis());
                                }
                                crate::switching::run_hook(self.switch_hook.clone(), "remote").await;
                                self.event_tx.send(ICaptureEvent::ClientReady(handle)).expect("channel closed");
                                while let Some(event) = self.buffered.pop_front() {
                                    if self.conn.send(ProtoEvent::Input(event), handle).await.is_err() {
                                        self.release_capture(capture, false).await?;
                                        break;
                                    }
                                }
                            }
                        }
                        // client disconnected
                        ProtoEvent::Leave(_) => {
                            log::info!("releasing capture: left remote client device region");
                            self.release_capture(capture, false).await?;
                        },
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::Release(center) => self.release_capture(capture, center).await?,
                    CaptureRequest::Enter(h) => {
                        if self.active_client.is_some() { continue; }
                        if self.captures.iter().any(|&(c, _, t)| c == h && t == CaptureType::Default) {
                            if !crate::switching::run_hook(self.switch_hook.clone(), "connecting").await {
                                crate::switching::run_hook(self.switch_hook.clone(), "error").await;
                                continue;
                            }
                            self.next_serial = self.next_serial.wrapping_add(1).max(1);
                            self.serial = self.next_serial;
                            self.started = Some(Instant::now());
                            capture.enter(h).await?;
                        } else {
                            log::warn!("enter: {h} is not an active client");
                            crate::switching::run_hook(self.switch_hook.clone(), "error").await;
                        self.event_tx.send(ICaptureEvent::ClientFailed).expect("channel closed");
                        }
                    }
                    CaptureRequest::Create(h, p, t) => {
                        self.add_capture(h, p, t);
                        if !self.hotkey_only || t != CaptureType::EnterOnly { capture.create(h, p).await?; }
                    }
                    CaptureRequest::Destroy(h) => {
                        if self.active_client == Some(h) {
                            self.release_capture(capture, false).await?;
                        }
                        let created = !self.hotkey_only || self.get_type(h) != CaptureType::EnterOnly;
                        self.remove_capture(h);
                        if created { capture.destroy(h).await?; }
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                },
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture, true).await;
        }

        if event == CaptureEvent::Begin {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle))
                .expect("channel closed");
        }

        // enter only capture (for incoming connections)
        if self.get_type(handle) == CaptureType::EnterOnly {
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            if !self.is_default_capture_at(self.get_pos(handle)) {
                log::info!("releasing capture: no active client at this position");
                capture.release().await?;
            }
            // we dont care about events from incoming handles except for releasing the capture
            return Ok(());
        }

        // activated a new client
        if event == CaptureEvent::Begin && Some(handle) != self.active_client {
            self.state = State::WaitingForAck;
            self.started.get_or_insert_with(Instant::now);
            self.active_client.replace(handle);
            self.event_tx
                .send(ICaptureEvent::ClientEntered(handle))
                .expect("channel closed");
        }

        let event = match event {
            CaptureEvent::Begin => self.enter_event(handle),
            CaptureEvent::Input(e) => match self.state {
                State::WaitingForAck => {
                    // Preserve initial keystrokes, but bound memory during a failed switch.
                    if self.buffered.len() >= 256 {
                        self.release_capture(capture, false).await?;
                        self.event_tx
                            .send(ICaptureEvent::ClientFailed)
                            .expect("channel closed");
                    } else {
                        self.buffered.push_back(e);
                    }
                    return Ok(());
                }
                State::Sending => ProtoEvent::Input(e),
            },
        };
        if let Err(e) = self.conn.send(event, handle).await {
            if self.state == State::Sending {
                log::warn!("switch connection lost: {e}");
                self.release_capture(capture, false).await?;
            }
            // During entry the timer retries without requiring mouse movement.
        }
        Ok(())
    }

    fn enter_event(&self, handle: CaptureHandle) -> ProtoEvent {
        let pos = to_proto_pos(self.get_pos(handle).opposite());
        if self.serial == 0 {
            ProtoEvent::Enter(pos)
        } else {
            ProtoEvent::HotkeyEnter {
                pos,
                serial: self.serial,
            }
        }
    }

    async fn release_capture(
        &mut self,
        capture: &mut InputCapture,
        center: bool,
    ) -> Result<(), CaptureError> {
        let pressed_keys = capture.take_pressed_keys();
        if center {
            capture.release_centered().await?;
        } else {
            capture.release().await?;
        }
        crate::switching::run_hook(self.switch_hook.clone(), "local").await;
        self.started = None;
        self.buffered.clear();
        self.state = State::default();
        let serial = std::mem::take(&mut self.serial);
        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
            // Synthesize key-up events for every key still held in the
            // capture's pressed_keys set BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP/DTLS.
            self.event_tx
                .send(ICaptureEvent::ClientLeft(handle))
                .expect("channel closed");
            let cleanup = async {
                for key in pressed_keys {
                    let key_up = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                        time: 0,
                        key: key as u32,
                        state: 0,
                    }));
                    if let Err(e) = self.conn.send(key_up, handle).await {
                        log::warn!("failed to send key-up to client {handle}: {e}");
                    }
                }
                // Reset the modifier mask too. The peer's input-emulation
                // layer keeps a separate XKB-style modifier state that's
                // updated by KeyboardEvent::Modifiers, distinct from the
                // pressed_keys set drained above. Without this, an
                // already-locked CapsLock would survive the release.
                let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                    depressed: 0,
                    latched: 0,
                    locked: 0,
                    group: 0,
                }));
                if let Err(e) = self.conn.send(mods_zero, handle).await {
                    log::warn!("failed to reset modifiers on client {handle}: {e}");
                }

                log::info!("sending Leave event to client {handle}");
                if let Err(e) = self.conn.send(ProtoEvent::Leave(serial), handle).await {
                    log::warn!("failed to send Leave to client {handle}: {e}");
                }
            };
            if tokio::time::timeout(Duration::from_millis(200), cleanup)
                .await
                .is_err()
            {
                log::warn!("remote key cleanup timed out; local capture already released");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

fn to_capture_pos(pos: lan_mouse_ipc::Position) -> input_capture::Position {
    match pos {
        lan_mouse_ipc::Position::Left => input_capture::Position::Left,
        lan_mouse_ipc::Position::Right => input_capture::Position::Right,
        lan_mouse_ipc::Position::Top => input_capture::Position::Top,
        lan_mouse_ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: input_capture::Position) -> lan_mouse_proto::Position {
    match pos {
        input_capture::Position::Left => lan_mouse_proto::Position::Left,
        input_capture::Position::Right => lan_mouse_proto::Position::Right,
        input_capture::Position::Top => lan_mouse_proto::Position::Top,
        input_capture::Position::Bottom => lan_mouse_proto::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}
