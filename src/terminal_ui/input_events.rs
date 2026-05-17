use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub enum InputEvent {
    Key(KeyEvent),
    Resize,
}

pub struct EventPump {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EventPump {
    pub fn start(sender: UnboundedSender<InputEvent>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_reader = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_reader.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => match event::read() {
                        Ok(Event::Key(key))
                            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                        {
                            let _ = sender.send(InputEvent::Key(key));
                        }
                        Ok(Event::Resize(_, _)) => {
                            let _ = sender.send(InputEvent::Resize);
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        });

        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for EventPump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
