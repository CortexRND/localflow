//! Worker-only Settings agent operations and their main-thread delivery data.

use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

use lf_core::config::Config;
use serde_json::Value;

use super::settings_agent::{
    ApplyResult, Change, ManualSetting, SettingsAgent, SettingsAgentError, UndoResult,
};

pub enum Request {
    Refresh,
    Apply(Vec<Change>),
    Undo,
    Manual {
        key: String,
        value: Value,
        typed: Option<String>,
    },
}

pub enum Outcome {
    Refreshed,
    Applied(Result<ApplyResult, SettingsAgentError>),
    Undone(Result<UndoResult, SettingsAgentError>),
    Manual {
        key: String,
        typed: Option<String>,
        result: Result<ApplyResult, SettingsAgentError>,
    },
}

pub struct Completion {
    pub outcome: Outcome,
    pub config: Result<Config, SettingsAgentError>,
    pub rows: Result<Vec<ManualSetting>, SettingsAgentError>,
    pub undo: Option<Vec<String>>,
}

pub fn dispatch(
    agent: Arc<dyn SettingsAgent>,
    request: Request,
    sender: mpsc::Sender<Completion>,
) -> std::io::Result<JoinHandle<()>> {
    let write =
        (!matches!(&request, Request::Refresh)).then(crate::shutdown::register_pending_write);
    thread::Builder::new()
        .name("localflow-settings-io".into())
        .spawn(move || {
            let outcome = crate::config_write::with_lock(|| match request {
                Request::Refresh => Outcome::Refreshed,
                Request::Apply(changes) => Outcome::Applied(agent.apply(&changes)),
                Request::Undo => Outcome::Undone(agent.undo()),
                Request::Manual { key, value, typed } => {
                    let result = agent.set_setting(&key, &value);
                    Outcome::Manual { key, typed, result }
                }
            });
            drop(write);
            let config = agent.load();
            let rows = config
                .as_ref()
                .map_err(|error| SettingsAgentError(error.0.clone()))
                .and_then(|config| agent.manual_settings(Some(config)));
            let undo = agent.pending_undo();
            let _ = sender.send(Completion {
                outcome,
                config,
                rows,
                undo,
            });
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::settings_agent::{FakeSettingsAgent, Proposal};
    use std::sync::Mutex;
    use std::time::Duration;

    struct CheckedAgent {
        inner: FakeSettingsAgent,
        owner: thread::ThreadId,
        entered: mpsc::Sender<()>,
        release: Mutex<Option<mpsc::Receiver<()>>>,
    }
    impl CheckedAgent {
        fn check(&self) {
            assert_ne!(thread::current().id(), self.owner);
        }
    }
    impl SettingsAgent for CheckedAgent {
        fn load(&self) -> Result<Config, SettingsAgentError> {
            self.check();
            let _ = self.entered.send(());
            if let Some(release) = self.release.lock().unwrap().take() {
                release.recv().unwrap();
            }
            self.inner.load()
        }
        fn current_settings(
            &self,
            config: Option<&Config>,
        ) -> Result<Vec<(String, String)>, SettingsAgentError> {
            self.check();
            self.inner.current_settings(config)
        }
        fn manual_settings(
            &self,
            config: Option<&Config>,
        ) -> Result<Vec<ManualSetting>, SettingsAgentError> {
            self.check();
            self.inner.manual_settings(config)
        }
        fn pending_undo(&self) -> Option<Vec<String>> {
            self.check();
            self.inner.pending_undo()
        }
        fn propose(&self, request: &str) -> Result<Proposal, SettingsAgentError> {
            self.check();
            self.inner.propose(request)
        }
        fn set_setting(&self, key: &str, value: &Value) -> Result<ApplyResult, SettingsAgentError> {
            self.check();
            self.inner.set_setting(key, value)
        }
        fn apply(&self, changes: &[Change]) -> Result<ApplyResult, SettingsAgentError> {
            self.check();
            self.inner.apply(changes)
        }
        fn undo(&self) -> Result<UndoResult, SettingsAgentError> {
            self.check();
            self.inner.undo()
        }
        fn model(&self) -> Option<String> {
            self.check();
            self.inner.model()
        }
    }

    #[test]
    fn blocked_refresh_and_all_writes_execute_off_the_calling_thread() {
        let (entered, started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let agent = Arc::new(CheckedAgent {
            inner: FakeSettingsAgent::new(),
            owner: thread::current().id(),
            entered,
            release: Mutex::new(Some(gate)),
        });
        let (sender, receiver) = mpsc::channel();
        let worker = dispatch(agent.clone(), Request::Refresh, sender.clone()).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        let result = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!result.rows.unwrap().is_empty());
        worker.join().unwrap();
        for request in [
            Request::Apply(Vec::new()),
            Request::Undo,
            Request::Manual {
                key: "sounds_enabled".into(),
                value: Value::Bool(false),
                typed: None,
            },
        ] {
            dispatch(agent.clone(), request, sender.clone())
                .unwrap()
                .join()
                .unwrap();
            assert!(receiver.recv().unwrap().config.is_ok());
        }
    }

    #[test]
    fn read_errors_and_empty_manual_rows_are_not_faked() {
        let agent = Arc::new(FakeSettingsAgent::with_rows(
            Config::defaults_for(true),
            Vec::new(),
        ));
        let (sender, receiver) = mpsc::channel();
        dispatch(agent.clone(), Request::Refresh, sender.clone())
            .unwrap()
            .join()
            .unwrap();
        assert!(receiver.recv().unwrap().rows.unwrap().is_empty());
        agent.set_load_error(Some("unreadable"));
        dispatch(agent, Request::Refresh, sender)
            .unwrap()
            .join()
            .unwrap();
        let completion = receiver.recv().unwrap();
        assert!(completion.config.is_err());
        assert!(completion.rows.is_err());
    }
}
