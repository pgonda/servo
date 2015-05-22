/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
extern crate time;
use time::Timespec;

use dom::bindings::cell::DOMRefCell;
use dom::bindings::callback::ExceptionHandling::Report;
use dom::bindings::codegen::Bindings::FunctionBinding::Function;
use dom::bindings::js::JSRef;
use dom::bindings::utils::Reflectable;

use dom::window::ScriptHelpers;

use script_task::{ScriptChan, ScriptMsg, TimerSource};
use horribly_inefficient_timers;

use util::task::spawn_named;
use util::str::DOMString;

use js::jsval::JSVal;

use std::borrow::ToOwned;
use std::cell::Cell;
use std::cmp;
use std::collections::{HashMap};
use std::hash::{Hash, Hasher};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::mpsc::Select;
use std::thread;

#[derive(PartialEq, Eq)]
#[jstraceable]
#[derive(Copy, Clone)]
pub struct TimerId(i32);

#[jstraceable]
#[privatize]
struct TimerHandle {
    handle: TimerId,
    data: TimerData,
    control_chan: Option<Sender<TimerControlMsg>>,
}

#[jstraceable]
#[derive(Clone)]
pub enum TimerCallback {
    StringTimerCallback(DOMString),
    FunctionTimerCallback(Function)
}

impl Hash for TimerId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let TimerId(id) = *self;
        id.hash(state);
    }
}

impl TimerHandle {
    fn cancel(&mut self) {
        self.control_chan.as_ref().map(|chan| chan.send(TimerControlMsg::Cancel).ok());
    }
    fn suspend(&mut self) {
        self.control_chan.as_ref().map(|chan| chan.send(TimerControlMsg::Suspend).ok());
    }
    fn resume(&mut self) {
        self.control_chan.as_ref().map(|chan| chan.send(TimerControlMsg::Resume).ok());
    }
}

#[jstraceable]
#[privatize]
pub struct TimerManager {
    active_timers: DOMRefCell<HashMap<TimerId, TimerHandle>>,
    next_timer_handle: Cell<i32>,
}


impl Drop for TimerManager {
    fn drop(&mut self) {
        for (_, timer_handle) in self.active_timers.borrow_mut().iter_mut() {
            timer_handle.cancel();
        }
    }
}

// Enum allowing more descriptive values for the is_interval field
#[jstraceable]
#[derive(PartialEq, Copy, Clone)]
pub enum IsInterval {
    Interval,
    NonInterval,
}

// Messages sent control timers from script task
#[jstraceable]
#[derive(PartialEq, Copy, Clone, Debug)]
pub enum TimerControlMsg {
    Cancel,
    Suspend,
    Resume
}

// Holder for the various JS values associated with setTimeout
// (ie. function value to invoke and all arguments to pass
//      to the function when calling it)
// TODO: Handle rooting during fire_timer when movable GC is turned on
#[jstraceable]
#[privatize]
#[derive(Clone)]
struct TimerData {
    is_interval: IsInterval,
    callback: TimerCallback,
    args: Vec<JSVal>
}

impl TimerManager {
    pub fn new() -> TimerManager {
        TimerManager {
            active_timers: DOMRefCell::new(HashMap::new()),
            next_timer_handle: Cell::new(0)
        }
    }

    pub fn suspend(&self) {
        for (_, timer_handle) in self.active_timers.borrow_mut().iter_mut() {
            timer_handle.suspend();
        }
    }
    pub fn resume(&self) {
        for (_, timer_handle) in self.active_timers.borrow_mut().iter_mut() {
            timer_handle.resume();
        }
    }

    #[allow(unsafe_code)]
    pub fn set_timeout_or_interval(&self,
                                  callback: TimerCallback,
                                  arguments: Vec<JSVal>,
                                  timeout: i32,
                                  is_interval: IsInterval,
                                  source: TimerSource,
                                  script_chan: Box<ScriptChan+Send>)
                                  -> i32 {
        let duration_ms = cmp::max(0, timeout) as u32;
        let handle = self.next_timer_handle.get();
        self.next_timer_handle.set(handle + 1);

        // Spawn a new timer task; it will dispatch the `ScriptMsg::FireTimer`
        // to the relevant script handler that will deal with it.
        let (control_chan, control_port) = channel();
        let spawn_name = match source {
            TimerSource::FromWindow(_) if is_interval == IsInterval::Interval => "Window:SetInterval",
            TimerSource::FromWorker if is_interval == IsInterval::Interval => "Worker:SetInterval",
            TimerSource::FromWindow(_) => "Window:SetTimeout",
            TimerSource::FromWorker => "Worker:SetTimeout",
        }.to_owned();
        spawn_named(spawn_name, move || {
            let timeout_port = if is_interval == IsInterval::Interval {
                horribly_inefficient_timers::periodic(duration_ms)
            } else {
                horribly_inefficient_timers::oneshot(duration_ms)
            };
            let control_port = control_port;

            let select = Select::new();
            let mut timeout_handle = select.handle(&timeout_port);
            unsafe { timeout_handle.add() };
            let mut control_handle = select.handle(&control_port);
            unsafe { control_handle.add() };

            loop {
                let id = select.wait();

                if id == timeout_handle.id() {
                    timeout_port.recv().unwrap();
                    if script_chan.send(ScriptMsg::FireTimer(source, TimerId(handle))).is_err() {
                        break;
                    }

                    if is_interval == IsInterval::NonInterval {
                        break;
                    }
                } else if id == control_handle.id() {;
                    match control_port.recv().unwrap() {
                        TimerControlMsg::Suspend => {
                            let msg = control_port.recv().unwrap();
                            match msg {
                                TimerControlMsg::Suspend => panic!("Nothing to suspend!"),
                                TimerControlMsg::Resume => {},
                                TimerControlMsg::Cancel => {
                                    break;
                                },
                            }
                            },
                        TimerControlMsg::Resume => panic!("Nothing to resume!"),
                        TimerControlMsg::Cancel => {
                            break;
                        }
                    }
                }
            }
        });
        let timer_id = TimerId(handle);
        let timer = TimerHandle {
            handle: timer_id,
            control_chan: Some(control_chan),
            data: TimerData {
                is_interval: is_interval,
                callback: callback,
                args: arguments
            }
        };
        self.active_timers.borrow_mut().insert(timer_id, timer);
        handle
    }

    pub fn clear_timeout_or_interval(&self, handle: i32) {
        let mut timer_handle = self.active_timers.borrow_mut().remove(&TimerId(handle));
        match timer_handle {
            Some(ref mut handle) => handle.cancel(),
            None => {}
        }
    }

    pub fn fire_timer<T: Reflectable>(&self, timer_id: TimerId, this: JSRef<T>) {

        let data = match self.active_timers.borrow().get(&timer_id) {
            None => return,
            Some(timer_handle) => timer_handle.data.clone(),
        };

        // TODO: Must handle rooting of funval and args when movable GC is turned on
        match data.callback {
            TimerCallback::FunctionTimerCallback(function) => {
                let _ = function.Call_(this, data.args, Report);
            }
            TimerCallback::StringTimerCallback(code_str) => {
                this.evaluate_js_on_global_with_result(&code_str);
            }
        };

        if data.is_interval == IsInterval::NonInterval {
            self.active_timers.borrow_mut().remove(&timer_id);
        }
    }
}

pub enum TimerRequest {
    Add { timeout: u32, tx: Sender<TimerResponse> },
    Remove { id: u32 },
}

pub enum TimerResponse {
    Timeout,
}

pub struct Timer {
    creation_time: Timespec,
    expiry_time: Timespec,
    tx: Sender<TimerResponse>,
    id: u32,
}

impl Timer {
    pub fn timespec_to_ms(ts: Timespec) {
        return ts.sec*1000 + ts.nsec/1000000;
    }
}

pub struct TimerEventLoop {
    timers: Vec<Timer>,
    control_port: Receiver<TimerRequest>,
}

impl TimerEventLoop {
    #[allow(unsafe_code)]
    pub fn start() -> Sender<TimerRequest> {
        let (tx, rx) = channel::<TimerRequest>();
        let mut event_loop = TimerEventLoop { timers: Vec::new(), control_port: rx };
        thread::spawn( move || {
            let timeout = u32::max_value();
            let select = Select::new();
            let mut timeout_port = horribly_inefficient_timers::oneshot(timeout);
            let mut timeout_handle = select.handle(&timeout_port);
            let mut timers: Vec<Timer> = Vec::new();
            unsafe { timeout_handle.add() };
            let mut control_handle = select.handle(&event_loop.control_port);
            unsafe { control_handle.add() };

            loop {
                let id = select.wait();

                if id == timeout_handle.id() {
                    timeout_handle.recv().unwrap();
                    let curr_time = time::get_time();
                    let (expired_timers, mut saved_timers): (Vec<Timer>, Vec<Timer>) =
                        timers.into_iter().partition(|t| t.creation_time.cmp(&curr_time) != cmp::Ordering::Greater);
                    for timer in expired_timers.iter() {
                        timer.tx.send(TimerResponse::Timeout);
                    }
                    if saved_timers.len() != 0 {
                        saved_timers.sort_by(|a, b| a.expiry_time.cmp(&b.expiry_time));
                        let timer = saved_timers.get(0).unwrap();
                        let timeout_port = horribly_inefficient_timers::oneshot(timeout);
                    } else {
                        let timeout_port = horribly_inefficient_timers::oneshot(timeout);
                    }
                   timeout_handle = select.handle(&timeout_port);
                    unsafe { timeout_handle.add() };
                    timers = saved_timers;
                } else if id ==control_handle.id() {
                    let int = match control_handle.recv().unwrap() {
                        TimerRequest::Add{timeout, tx} => {
                            let curr_time = time::get_time();
                            let duration_to_timeout = time::Duration::milliseconds(timeout);
                            let timeout = curr_time.clone().add(duration_to_timeout as i64);
                            let timer = Timer { creation_time: curr_time, expiry_time: timeout, tx: tx, id: 1 };
                            if (timers.len() != 0) & (timers.get(0).unwrap().expiry_time.cmp(&timer.expiry_time) == cmp::Ordering::Greater) {
                                let timeout_port = horribly_inefficient_timers::oneshot(Timer::timespec_to_ms(timer.expiry_time));
                                timeout_handle = select.handle(&timeout_port);
                                unsafe { timeout_handle.add() };
                            }
                            timers.push(timer);
                            timers.sort_by(|a, b| a.expiry_time.cmp(&b.expiry_time));
                            1
                        },
                        TimerRequest::Remove{id} => 2,
                    };
                }
            }
        });
        tx
    }
}
