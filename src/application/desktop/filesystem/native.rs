//! Reserved G-native control runs independently of F and the ordinary relay.
use super::*;
use crate::catalog_session::{native as n, preview_stage as stage};

pub(super) struct Stages {
    filesystem: Arc<Client>,
    selected: Mutex<Option<Arc<crate::preview::stage_io::Calls>>>,
}
impl Stages {
    fn calls(&self, root: &RootCapability) -> Result<Arc<crate::preview::stage_io::Calls>> {
        let mut selected = self.selected.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(calls) = selected.as_ref() {
            if calls.root == *root {
                return Ok(calls.clone());
            }
            calls.reconcile()?;
        }
        let calls =
            crate::preview::stage_io::Calls::new(self.filesystem.clone(), root.clone(), true);
        *selected = Some(calls.clone());
        Ok(calls)
    }
}
impl super::super::native::Stages for Stages {
    fn abandon(&self, root: &RootCapability) -> Result<()> {
        let calls = self.calls(root)?;
        calls.reconcile()?;
        calls.unit(stage::Action::AbandonOwned)
    }
    fn arm(
        &self,
        root: &RootCapability,
        stage: &LeaseId,
        operation: U64,
    ) -> Result<std::path::PathBuf> {
        let calls = self.calls(root)?;
        calls.reconcile()?;
        match calls.call(
            stage::Action::Arm {
                stage: stage.clone(),
                native: operation,
            },
            &AtomicBool::new(false),
        )? {
            stage::Value::Path(path) => Ok(path.to_path()?),
            _ => anyhow::bail!("wrong native stage arm reply"),
        }
    }
    fn drained(&self, root: &RootCapability, stage: &LeaseId, operation: U64) -> Result<()> {
        let calls = self.calls(root)?;
        calls.reconcile()?;
        calls.unit(stage::Action::NativeDrained {
            stage: stage.clone(),
            native: operation,
        })
    }
    fn header(&self, root: &RootCapability, stage: &LeaseId, operation: U64) -> Result<n::Header> {
        let bytes = self
            .calls(root)?
            .metadata(stage, stage::Artifact::Header)?
            .context("native header not ready")?;
        let header: n::Header = serde_json::from_slice(&bytes)?;
        header.validate()?;
        ensure!(
            header.operation == operation && header.stage == *stage,
            "native stage header binding"
        );
        Ok(header)
    }
}
impl Parent {
    pub fn configure_native(
        &self,
        executable: std::path::PathBuf,
        limits: crate::preview::ServiceLimits,
        budget: &crate::preview::ByteBudget,
    ) -> Result<()> {
        let mut native = self.native.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(native.is_none(), "native owner already configured");
        *native = Some(Arc::new(super::super::native::Owner::new(
            executable,
            Arc::new(Stages {
                filesystem: self.client.clone(),
                selected: Mutex::new(None),
            }),
            limits,
            budget,
        )?));
        Ok(())
    }
    pub(super) fn native_owner(&self) -> Result<Arc<super::super::native::Owner>> {
        self.native
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .context("managed native custody not configured")
    }
    pub(super) fn receive_native(&self, body: &Body) -> Result<bool> {
        match body {
            Body::Control(Control::NativeStop { key }) => {
                self.native_owner()?.stop_key(key)?;
                Ok(true)
            }
            Body::Control(Control::NativeQuery { id, query }) => {
                query.key.validate()?;
                ensure!(query.key.epoch == self.binding.epoch, "native query epoch");
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some((old_id, old_query, reply)) = &state.native_result
                    && *old_id == id.0
                {
                    ensure!(old_query == query, "changed native control replay");
                    let reply = reply.clone();
                    state.output.push(&self.binding, reply)?;
                    return Ok(true);
                }
                ensure!(id.0 == state.native_next, "native query replay/gap");
                let next = id.0.checked_add(1).context("native query exhausted")?;
                // Owner query performs cached/short-lock operations only. F
                // acknowledgements run on its retained reaper, never this lane.
                let value = self
                    .native_owner()
                    .and_then(|n| n.query(query))
                    .map_err(|e| Fault::from_error(e, false));
                let reply = Body::Control(Control::NativeReply {
                    id: *id,
                    query: query.clone(),
                    value,
                });
                state.output.push(&self.binding, reply.clone())?;
                state.native_next = next;
                state.native_result = Some((id.0, query.clone(), reply));
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}
pub(super) struct Pending {
    id: u64,
    query: n::Query,
    result: Option<std::result::Result<n::Status, Fault>>,
}
impl Proxy {
    pub(super) fn native_query(&self, query: n::Query) -> Result<n::Status> {
        query.key.validate()?;
        ensure!(query.key.epoch == self.binding.epoch, "native query epoch");
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            !state.native_dead,
            "native supervisor transport ended; ownership unresolved"
        );
        ensure!(
            state.native_query.is_none(),
            crate::preview::stage_io::Busy("native control query busy; retry")
        );
        let id = state.native_next;
        let next = id.checked_add(1).context("native query exhausted")?;
        state.output.push(
            &self.binding,
            Body::Control(Control::NativeQuery {
                id: U64(id),
                query: query.clone(),
            }),
        )?;
        state.native_next = next;
        state.native_query = Some(Pending {
            id,
            query,
            result: None,
        });
        loop {
            if state
                .native_query
                .as_ref()
                .is_some_and(|q| q.result.is_some())
            {
                return state
                    .native_query
                    .take()
                    .unwrap()
                    .result
                    .unwrap()
                    .map_err(Fault::into_error);
            }
            ensure!(
                !state.native_dead,
                "native supervisor transport ended; ownership unresolved"
            );
            state = self.wake.wait(state).unwrap_or_else(|p| p.into_inner());
        }
    }
    pub(super) fn receive_native_reply(&self, body: &Body) -> Result<bool> {
        let Body::Control(Control::NativeReply { id, query, value }) = body else {
            return Ok(false);
        };
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let pending = state
            .native_query
            .as_mut()
            .context("unsolicited native status")?;
        ensure!(
            pending.id == id.0 && pending.query == *query && pending.result.is_none(),
            "native reply identity"
        );
        match value {
            Ok(v) => {
                ensure!(
                    v.epoch == query.key.epoch
                        && v.session == query.key.session
                        && v.operation == query.key.operation,
                    "native status binding"
                );
                ensure!(
                    v.error.as_ref().is_none_or(|e| e.len() <= n::ERROR_BYTES),
                    "native error bounds"
                );
            }
            Err(e) => e.validate()?,
        }
        pending.result = Some(value.clone());
        self.wake.notify_all();
        Ok(true)
    }
}
impl n::CatalogNative for Proxy {
    fn signal_stop(&self, root: &RootCapability, operation: U64) -> Result<()> {
        let key = n::Key::new(root, operation);
        key.validate()?;
        ensure!(
            key.epoch == self.binding.epoch,
            "native Stop epoch mismatch"
        );
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(!state.native_dead, "native supervisor transport ended");
        state
            .output
            .push(&self.binding, Body::Control(Control::NativeStop { key }))
    }

    fn call(&self, request: &n::Request, cancel: &AtomicBool) -> Result<n::Status> {
        request.validate()?;
        let key = n::Key::new(&request.root, request.operation);
        match request.action {
            n::Action::Stop => {
                {
                    let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    ensure!(!state.native_dead, "native supervisor transport ended");
                    state.output.push(
                        &self.binding,
                        Body::Control(Control::NativeStop { key: key.clone() }),
                    )?;
                }
                self.native_query(n::Query {
                    key,
                    action: n::QueryAction::Status,
                })
            }
            n::Action::Drain => self.native_query(n::Query {
                key,
                action: n::QueryAction::RetryDrain,
            }),
            n::Action::Retire => self.native_query(n::Query {
                key,
                action: n::QueryAction::Retire,
            }),
            _ => match self.call(Call::Native(Box::new(request.clone())), cancel)? {
                Value::Native(v) => {
                    v.validate(&request.root, request.operation)?;
                    Ok(v)
                }
                _ => anyhow::bail!("wrong native reply"),
            },
        }
    }
    fn status(&self, root: &RootCapability, operation: U64) -> Result<n::Status> {
        self.native_query(n::Query {
            key: n::Key::new(root, operation),
            action: n::QueryAction::Status,
        })
    }
}
