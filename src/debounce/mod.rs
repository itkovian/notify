#![allow(missing_docs)]

mod timer;

use super::{op, Error, Event as NotifyEvent};

use self::timer::WatchTimer;

use std::sync::mpsc;
use std::path::PathBuf;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type OperationsBuffer = Arc<Mutex<HashMap<PathBuf, (Option<op::Op>, Option<PathBuf>, Option<u64>)>>>;

#[derive(Debug)]
/// Events emitted by `notify` in _debounced_ mode.
pub enum Event {
    /// `NoticeWrite` is emitted imediatelly after the first write event for the path.
    ///
    /// If you are reading from that file, you should probably close it imediatelly and discard all data you read from it.
    NoticeWrite(PathBuf),
    /// `NoticeRemove` is emitted imediatelly after a remove or rename event for the path.
    ///
    /// The file will continue to exist until its last file handle is closed.
    NoticeRemove(PathBuf),
    /// `Create` is emitted when a file or directory has been created and no events were detected for the path within the specified time frame.
    ///
    /// `Create` events have a higher priority than `Write` and `Chmod`.
    /// These events will not be emitted if they are detected before the `Create` event has been emitted.
    Create(PathBuf),
    /// `Write` is emitted when a file has been written to and no events were detected for the path within the specified time frame.
    ///
    /// `Write` events have a higher priority than `Chmod`.
    /// `Chmod` will not be emitted if it's detected before the `Write` event has been emitted.
    ///
    /// Upon receiving a `Create` event for a directory, it is necessary to scan the newly created directory for contents.
    /// The directory can contain files or directories if those contents were created before the directory could be watched,
    /// or if the directory was moved into the watched directory.
    Write(PathBuf),
    /// `Chmod` is emitted when attributes have been changed and no events were detected for the path within the specified time frame.
    Chmod(PathBuf),
    /// `Remove` is emitted when a file or directory has been removed and no events were detected for the path within the specified time frame.
    Remove(PathBuf),
    /// `Rename` is emitted when a file or directory has been moved within a watched directory and no events were detected for the new path within the specified time frame.
    ///
    /// The first path contains the source, the second path the destination.
    Rename(PathBuf, PathBuf),
    /// `Rescan` is emitted imediatelly after a problem has been detected that makes it necessary to re-scan the watched directories.
    Rescan,
    /// `Error` is emitted imediatelly after a error has been detected.
    ///
    ///  This event may contain a path for which the error was detected.
    Error(Error, Option<PathBuf>),
}

impl PartialEq for Event {
    fn eq(&self, other: &Event) -> bool {
        match (self, other) {
            (&Event::NoticeWrite(ref a), &Event::NoticeWrite(ref b)) |
            (&Event::NoticeRemove(ref a), &Event::NoticeRemove(ref b)) |
            (&Event::Create(ref a), &Event::Create(ref b)) |
            (&Event::Write(ref a), &Event::Write(ref b)) |
            (&Event::Chmod(ref a), &Event::Chmod(ref b)) |
            (&Event::Remove(ref a), &Event::Remove(ref b)) => a == b,
            (&Event::Rename(ref a1, ref a2), &Event::Rename(ref b1, ref b2)) => (a1 == b1 && a2 == b2),
            (&Event::Rescan, &Event::Rescan) => true,
            _ => false,
        }
    }
}

pub enum EventTx {
    Raw {
        tx: mpsc::Sender<NotifyEvent>,
    },
    Debounced {
        tx: mpsc::Sender<Event>,
        debounce: Debounce,
    },
    DebouncedTx {
        tx: mpsc::Sender<Event>,
    },
}

impl EventTx {
    pub fn send(&mut self, event: NotifyEvent) {
        match *self {
            EventTx::Raw { ref tx } => {
                let _ = tx.send(event);
            }
            EventTx::Debounced { ref tx, ref mut debounce } => {
                match (event.path, event.op, event.cookie) {
                    (None, Ok(op::RESCAN), None) => {
                        let _ = tx.send(Event::Rescan);
                    }
                    (Some(path), Ok(op), cookie) => {
                        debounce.event(path, op, cookie);
                    }
                    (None, Ok(_op), _cookie) => {
                        // TODO panic!("path is None: {:?} ({:?})", _op, _cookie);
                    }
                    (path, Err(e), _) => {
                        let _ = tx.send(Event::Error(e, path));
                    }
                }
            }
            EventTx::DebouncedTx { ref tx } => {
                match (event.path, event.op, event.cookie) {
                    (None, Ok(op::RESCAN), None) => {
                        let _ = tx.send(Event::Rescan);
                    }
                    (Some(_path), Ok(_op), _cookie) => {
                        // TODO debounce.event(_path, _op, _cookie);
                    }
                    (None, Ok(_op), _cookie) => {
                        // TODO panic!("path is None: {:?} ({:?})", _op, _cookie);
                    }
                    (path, Err(e), _) => {
                        let _ = tx.send(Event::Error(e, path));
                    }
                }
            }
        }
    }
}

pub struct Debounce {
    tx: mpsc::Sender<Event>,
    operations_buffer: OperationsBuffer,
    rename_path: Option<PathBuf>,
    rename_cookie: Option<u32>,
    timer: WatchTimer,
}

impl Debounce {
    pub fn new(delay: Duration, tx: mpsc::Sender<Event>) -> Debounce {
        let operations_buffer: OperationsBuffer = Arc::new(Mutex::new(HashMap::new()));

        // spawns new thread
        let timer = WatchTimer::new(tx.clone(), operations_buffer.clone(), delay);

        Debounce {
            tx: tx,
            operations_buffer: operations_buffer,
            rename_path: None,
            rename_cookie: None,
            timer: timer,
        }
    }

    fn check_partial_rename(&mut self, path: PathBuf, op: op::Op, cookie: Option<u32>) {
        if let Ok(mut op_buf) = self.operations_buffer.lock() {
            // the previous event was a rename event, but this one isn't; something went wrong
            let mut remove_path: Option<PathBuf> = None;
            {
                let &mut (ref mut operation, ref mut from_path, ref mut timer_id) = op_buf.get_mut(self.rename_path.as_ref().unwrap()).expect("rename_path is set but not present in operations_buffer 1");
                if op != op::RENAME || self.rename_cookie.is_none() || self.rename_cookie != cookie {
                    if self.rename_path.as_ref().unwrap().exists() {
                        match *operation {
                            Some(op::RENAME) if from_path.is_none() => {
                                // file has been moved into the watched directory
                                *operation = Some(op::CREATE);
                                restart_timer(timer_id, path, &mut self.timer);
                            }
                            Some(op::REMOVE) => {
                                // file has been moved removed before and has now been moved into the watched directory
                                *operation = Some(op::WRITE);
                                restart_timer(timer_id, path, &mut self.timer);
                            }
                            _ => {
                                unreachable!();
                            }
                        }
                    } else {
                        match *operation {
                            Some(op::CREATE) => {
                                // file was just created, so just remove the operations_buffer entry / no need to emit NoticeRemove because the file has just been created
                                // ignore running timer
                                if let Some(timer_id) = *timer_id {
                                    self.timer.ignore(timer_id);
                                }
                                // remember for deletion
                                remove_path = Some(path);
                            }
                            Some(op::WRITE) | // change to remove event
                            Some(op::CHMOD) => { // change to remove event
                                *operation = Some(op::REMOVE);
                                let _ = self.tx.send(Event::NoticeRemove(path.clone()));
                                restart_timer(timer_id, path, &mut self.timer);
                            }
                            Some(op::RENAME) => {
                                // file has been renamed before, change to remove event / no need to emit NoticeRemove because the file has been renamed before
                                *operation = Some(op::REMOVE);
                                restart_timer(timer_id, path, &mut self.timer);
                            }
                            // renaming a deleted file is impossible
                            _ => {
                                unreachable!();
                            }
                        }
                    }
                    self.rename_path = None;
                }
            }
            if let Some(path) = remove_path {
                op_buf.remove(&path);
            }
        }
    }

    pub fn event(&mut self, path: PathBuf, mut op: op::Op, cookie: Option<u32>) {
        if op.contains(op::RESCAN) {
            let _ = self.tx.send(Event::Rescan);
        }

        if self.rename_path.is_some() {
            self.check_partial_rename(path.clone(), op, cookie);
        }

        if let Ok(mut op_buf) = self.operations_buffer.lock() {
            if let Some(&(ref operation, _, _)) = op_buf.get(&path) {
                op = remove_repeated_events(op, operation);
            } else if op.contains(op::CREATE | op::REMOVE) {
                if path.exists() {
                    op.remove(op::REMOVE);
                } else {
                    op.remove(op::CREATE);
                }
            }

            if op.contains(op::CREATE) {
                let &mut (ref mut operation, _, ref mut timer_id) = op_buf.entry(path.clone()).or_insert((None, None, None));
                match *operation {
                    Some(op::CREATE) | // file can't be created twice
                    Some(op::WRITE) | // file can't be written to before being created
                    Some(op::CHMOD) | // file can't be changed before being created
                    Some(op::RENAME) => { // file can't be renamed to before being created
                        unreachable!(); // (repetitions are removed anyway)
                    }
                    Some(op::REMOVE) => {
                        // file has been removed and is now being re-created; convert this to a write event
                        *operation = Some(op::WRITE);
                        restart_timer(timer_id, path.clone(), &mut self.timer);
                    }
                    None => {
                        // operations_buffer entry didn't exist
                        *operation = Some(op::CREATE);
                        restart_timer(timer_id, path.clone(), &mut self.timer);
                    }
                    _ => {
                        unreachable!();
                    }
                }
            }
            if op.contains(op::WRITE) {
                let &mut (ref mut operation, _, ref mut timer_id) = op_buf.entry(path.clone()).or_insert((None, None, None));
                match *operation {
                    Some(op::CREATE) | // keep create event / no need to emit NoticeWrite because the file has just been created
                    Some(op::WRITE) => { // keep write event / not need to emit NoticeWrite because is already was a write event
                        restart_timer(timer_id, path.clone(), &mut self.timer);
                    }
                    Some(op::CHMOD) | // upgrade to write event
                    Some(op::RENAME) | // file has been renamed before, upgrade to write event
                    None => { // operations_buffer entry didn't exist
                        *operation = Some(op::WRITE);
                        let _ = self.tx.send(Event::NoticeWrite(path.clone()));
                        restart_timer(timer_id, path.clone(), &mut self.timer);
                    }
                    // writing to a deleted file is impossible
                    _ => {
                        unreachable!();
                    }
                }
            }
            if op.contains(op::CHMOD) {
                let &mut (ref mut operation, _, ref mut timer_id) = op_buf.entry(path.clone()).or_insert((None, None, None));
                match *operation {
                    Some(op::CREATE) | // keep create event
                    Some(op::WRITE) | // keep write event
                    Some(op::CHMOD) => { // keep chmod event
                        restart_timer(timer_id, path.clone(), &mut self.timer);
                    }
                    Some(op::RENAME) | // file has been renamed before, upgrade to chmod event
                    None => { // operations_buffer entry didn't exist

                        *operation = Some(op::CHMOD);
                        restart_timer(timer_id, path.clone(), &mut self.timer);
                    }
                    // changing a deleted file is impossible
                    _ => {
                        unreachable!();
                    }
                }
            }
            if op.contains(op::RENAME) {
                if self.rename_path.is_some() && self.rename_cookie.is_some() && self.rename_cookie == cookie {
                    // this is the second part of a rename operation, the old path is stored in the rename_path variable
                    // unwrap is safe because rename_path is some
                    let (from_operation, from_from_path, from_timer_id) = op_buf.remove(self.rename_path.as_ref().unwrap()).expect("rename_path is set but not present in operations_buffer");
                    // ignore running timer of removed operations_buffer entry
                    if let Some(from_timer_id) = from_timer_id {
                        self.timer.ignore(from_timer_id);
                    }
                    let use_from_path = from_from_path.or(self.rename_path.clone()); // if the file has been renamed before, use original name as from_path
                    let &mut (ref mut operation, ref mut from_path, ref mut timer_id) = op_buf.entry(path.clone()).or_insert((None, None, None));
                    match from_operation {
                        Some(op::CREATE) => {
                            // file has just been created, so move the create event to the new path
                            *operation = from_operation;
                            *from_path = None;
                            restart_timer(timer_id, path.clone(), &mut self.timer);
                        }
                        Some(op::WRITE) | // file has been written to, so move the event to the new path, but keep the write event
                        Some(op::CHMOD) | // file has been changed, so move the event to the new path, but keep the chmod event
                        Some(op::RENAME) => { // file has been renamed before, so move the event to the new path and update the from_path
                            *operation = from_operation;
                            *from_path = use_from_path;
                            restart_timer(timer_id, path.clone(), &mut self.timer);
                        }
                        // file can't be renamed after beeing removed
                        _ => {
                            unreachable!();
                        }
                    }
                    // reset the rename_path
                    self.rename_path = None;
                } else {
                    // this is the first part of a rename operation, store path for the subsequent rename event
                    self.rename_path = Some(path.clone());
                    self.rename_cookie = cookie;

                    let &mut (ref mut operation, _, ref mut timer_id) = op_buf.entry(path.clone()).or_insert((None, None, None));
                    match *operation {
                        Some(op::CREATE) | // keep create event / no need to emit NoticeRemove because the file has just been created
                        Some(op::RENAME) => { // file has been renamed before, keep rename event / no need to emit NoticeRemove because the file has been renamed before
                            restart_timer(timer_id, path.clone(), &mut self.timer);
                        }
                        Some(op::WRITE) | // keep write event
                        Some(op::CHMOD) => { // keep chmod event
                            let _ = self.tx.send(Event::NoticeRemove(path.clone()));
                            restart_timer(timer_id, path.clone(), &mut self.timer);
                        }
                        None => {
                            // operations_buffer entry didn't exist
                            *operation = Some(op::RENAME);
                            let _ = self.tx.send(Event::NoticeRemove(path.clone()));
                            restart_timer(timer_id, path.clone(), &mut self.timer);
                        }
                        // renaming a deleted file is impossible
                        _ => {
                            unreachable!();
                        }
                    }
                }
            }
            if op.contains(op::REMOVE) {
                let mut remove_path: Option<PathBuf> = None;
                {
                    let &mut (ref mut operation, _, ref mut timer_id) = op_buf.entry(path.clone()).or_insert((None, None, None));
                    match *operation {
                        Some(op::CREATE) => {
                            // file was just created, so just remove the operations_buffer entry / no need to emit NoticeRemove because the file has just been created
                            // ignore running timer
                            if let Some(timer_id) = *timer_id {
                                self.timer.ignore(timer_id);
                            }
                            // remember for deletion
                            remove_path = Some(path.clone());
                        }
                        Some(op::WRITE) | // change to remove event
                        Some(op::CHMOD) | // change to remove event
                        None => { // operations_buffer entry didn't exist
                            *operation = Some(op::REMOVE);
                            let _ = self.tx.send(Event::NoticeRemove(path.clone()));
                            restart_timer(timer_id, path.clone(), &mut self.timer);
                        }
                        Some(op::RENAME) => {
                            // file has been renamed before, change to remove event / no need to emit NoticeRemove because the file has been renamed before
                            *operation = Some(op::REMOVE);
                            restart_timer(timer_id, path.clone(), &mut self.timer);
                        }
                        Some(op::REMOVE) => {
                            // multiple remove events are possible if the file/directory is itself watched and in a watched directory
                        }
                        _ => {
                            unreachable!();
                        }
                    }
                }
                if let Some(path) = remove_path {
                    op_buf.remove(&path);
                    if self.rename_path == Some(path) {
                        self.rename_path = None;
                    }
                }
            }
        }
    }
}

fn remove_repeated_events(mut op: op::Op, prev_op: &Option<op::Op>) -> op::Op {
    if let Some(prev_op) = *prev_op {
        if prev_op.intersects(op::CREATE | op::WRITE | op::CHMOD | op::RENAME) {
            op.remove(op::CREATE);
        }

        if prev_op.contains(op::REMOVE) {
            op.remove(op::REMOVE);
        }

        if prev_op.contains(op::RENAME) && op & !op::RENAME != op::Op::empty() {
            op.remove(op::RENAME);
        }
    }
    op
}

fn restart_timer(timer_id: &mut Option<u64>, path: PathBuf, timer: &mut WatchTimer) {
    if let Some(timer_id) = *timer_id {
        timer.ignore(timer_id);
    }
    *timer_id = Some(timer.schedule(path));
}