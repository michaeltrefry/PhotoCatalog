//! Export-native lifecycle/control relay and G-to-F stage adapter.
use super::*;
use crate::catalog_session::{export_native as n, export_stage};

pub(super) struct Stages {
    filesystem: Arc<Client>,
}
impl super::super::export_native::Stages for Stages {
    fn call(
        &self,
        request: &export_stage::Request,
        cancel: &AtomicBool,
    ) -> Result<export_stage::Reply> {
        self.filesystem.export_stage_call(request, cancel)
    }
}

impl Parent {
    pub fn configure_export_native(
        &self,
        executable: std::path::PathBuf,
        max_slots: usize,
        budget: &crate::preview::ByteBudget,
    ) -> Result<()> {
        let mut owner = self.export_native.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(owner.is_none(), "export native owner already configured");
        *owner = Some(Arc::new(super::super::export_native::Owner::new(
            executable,
            Arc::new(Stages {
                filesystem: self.client.clone(),
            }),
            max_slots,
            budget,
        )?));
        Ok(())
    }
    pub(super) fn export_native_owner(&self) -> Result<Arc<super::super::export_native::Owner>> {
        self.export_native
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .context("managed export native custody not configured")
    }
    pub(super) fn receive_export_native(&self, body: &Body) -> Result<bool> {
        match body {
            Body::Control(Control::ExportNativeStop { key }) => {
                self.export_native_owner()?.stop_key(key)?;
                Ok(true)
            }
            Body::Control(Control::ExportNativeQuery { id, query }) => {
                query.key.validate()?;
                ensure!(
                    query.key.epoch == self.binding.epoch,
                    "export native query epoch"
                );
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some((old_id, old_query, reply)) = &state.export_native_result
                    && *old_id == id.0
                {
                    ensure!(old_query == query, "changed export native control replay");
                    let reply = reply.clone();
                    state.output.push(&self.binding, reply)?;
                    return Ok(true);
                }
                ensure!(
                    id.0 == state.export_native_next,
                    "export native query replay/gap"
                );
                let next =
                    id.0.checked_add(1)
                        .context("export native query exhausted")?;
                // A contiguous query proves C consumed the prior result. Any
                // queued duplicate of that completed result can be superseded
                // in this bounded class before computing/enqueuing the next one.
                if let Some((old_id, _, _)) = &state.export_native_result {
                    let old_id = *old_id;
                    state
                        .output
                        .export_native_reply
                        .entries
                        .retain(|(queued, _)| *queued != old_id);
                }
                let value = self
                    .export_native_owner()
                    .and_then(|owner| owner.query(query))
                    .map_err(|error| {
                        let busy = error
                            .downcast_ref::<crate::preview::stage_io::Busy>()
                            .is_some();
                        let mut fault = Fault::from_error(error, false);
                        fault.busy = busy;
                        fault
                    });
                let reply = Body::Control(Control::ExportNativeReply {
                    id: *id,
                    query: query.clone(),
                    value,
                });
                state.export_native_next = next;
                state.export_native_result = Some((id.0, query.clone(), reply.clone()));
                state.output.push(&self.binding, reply)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[derive(Clone)]
pub(super) struct Pending {
    id: u64,
    active: bool,
    query: n::Query,
    result: Option<std::result::Result<n::Status, Fault>>,
}
impl Proxy {
    fn export_native_query(&self, query: n::Query) -> Result<n::Status> {
        query.key.validate()?;
        ensure!(
            query.key.epoch == self.binding.epoch,
            "export native query epoch"
        );
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            !state.export_native_dead,
            "export native supervisor transport ended; ownership unresolved"
        );
        if let Some(pending) = &state.export_native_query {
            ensure!(
                !pending.active && pending.query == query,
                crate::preview::stage_io::Busy(
                    "export native control query busy; retry exact query"
                )
            );
        } else {
            let id = state.export_native_next;
            let next = id.checked_add(1).context("export native query exhausted")?;
            state.export_native_query = Some(Pending {
                id,
                query,
                active: false,
                result: None,
            });
            state.export_native_next = next;
        }
        state.export_native_query.as_mut().unwrap().active = true;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let pending = state.export_native_query.as_ref().unwrap();
            if let Some(result) = pending.result.clone() {
                state.export_native_last = state.export_native_query.take();
                return result.map_err(Fault::into_error);
            }
            if state.export_native_dead || Instant::now() >= deadline {
                state.export_native_query.as_mut().unwrap().active = false;
                anyhow::bail!("export native control reply unresolved; retry the exact query");
            }
            let body = Body::Control(Control::ExportNativeQuery {
                id: U64(pending.id),
                query: pending.query.clone(),
            });
            if let Err(error) = state.output.push(&self.binding, body) {
                state.export_native_query.as_mut().unwrap().active = false;
                return Err(error);
            }
            // The one bounded output slot deduplicates unsent copies. Once a
            // copy has left G/C, resend the same identity until its ACK arrives.
            state = self
                .wake
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
    pub(super) fn receive_export_native_reply(&self, body: &Body) -> Result<bool> {
        let Body::Control(Control::ExportNativeReply { id, query, value }) = body else {
            return Ok(false);
        };
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(last) = &state.export_native_last
            && last.id == id.0
        {
            ensure!(
                last.query == *query
                    && last.result.as_ref().is_some_and(
                        |old| serde_json::to_vec(old).ok() == serde_json::to_vec(value).ok()
                    ),
                "changed completed export native control reply"
            );
            return Ok(true);
        }
        let pending = state
            .export_native_query
            .as_mut()
            .context("unsolicited export native status")?;
        ensure!(
            pending.id == id.0 && pending.query == *query,
            "export native reply identity"
        );
        if let Some(old) = &pending.result {
            ensure!(
                serde_json::to_vec(old)? == serde_json::to_vec(value)?,
                "changed duplicate export native reply"
            );
            return Ok(true);
        }
        match value {
            Ok(status) => {
                status.binding.validate()?;
                ensure!(
                    status
                        .error
                        .as_ref()
                        .is_none_or(|error| error.len() <= n::ERROR_BYTES),
                    "export native status error limit"
                );
                ensure!(
                    status.epoch == query.key.epoch
                        && status.session == query.key.session
                        && status.operation == query.key.operation
                        && status.stage == query.key.stage,
                    "export native status binding"
                );
            }
            Err(error) => error.validate()?,
        }
        pending.result = Some(value.clone());
        self.wake.notify_all();
        Ok(true)
    }
}

impl n::CatalogExportNative for Proxy {
    fn signal_stop(&self, request: &n::Request) -> Result<()> {
        request.validate()?;
        ensure!(
            matches!(request.action, n::Action::Stop),
            "export native stop action"
        );
        let key = n::Key::new(&request.root, request.operation, &request.stage);
        ensure!(key.epoch == self.binding.epoch, "export native stop epoch");
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            !state.export_native_dead,
            "export native supervisor transport ended"
        );
        state.output.push(
            &self.binding,
            Body::Control(Control::ExportNativeStop { key }),
        )
    }
    fn call(&self, request: &n::Request, cancel: &AtomicBool) -> Result<n::Status> {
        request.validate()?;
        let key = n::Key::new(&request.root, request.operation, &request.stage);
        match request.action {
            n::Action::Stop => {
                self.signal_stop(request)?;
                self.export_native_query(n::Query {
                    key,
                    action: n::QueryAction::Status,
                })
            }
            n::Action::RetryDrain => self.export_native_query(n::Query {
                key,
                action: n::QueryAction::RetryDrain,
            }),
            n::Action::Retire => self.export_native_query(n::Query {
                key,
                action: n::QueryAction::Retire,
            }),
            _ => match self.call(Call::ExportNative(Box::new(request.clone())), cancel)? {
                Value::ExportNative(status) => {
                    status.validate(request)?;
                    Ok(status)
                }
                _ => anyhow::bail!("wrong export native reply"),
            },
        }
    }
    fn status(&self, key: &n::Key) -> Result<n::Status> {
        self.export_native_query(n::Query {
            key: key.clone(),
            action: n::QueryAction::Status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maximum_complete_export_envelopes_survive_pending_and_replay_lifetimes() -> Result<()> {
        use crate::application::desktop::export_native::{
            Owner,
            tests::{FakeStages, fixture, register},
        };
        let mut f = fixture()?;
        // Largest legal root path, exact persisted plan, independent blob
        // declarations and maximum binary chunk retain their full envelopes.
        f.root.canonical_root =
            crate::storage_volume::NativePath::from_path(&std::path::PathBuf::from(format!(
                "/{}",
                "x".repeat(crate::catalog_session::PATH_UNITS - 1)
            )));
        f.begin.root = f.root.clone();
        let export_stage::Action::Begin { work, .. } = &mut f.begin.action else {
            unreachable!()
        };
        let raw = format!(
            "{}{}",
            "\n".repeat(export_stage::PLAN_BYTES - work.plan.raw().len()),
            work.plan.raw()
        );
        work.authority = blake3::hash(raw.as_bytes()).to_hex().to_string();
        work.plan = crate::catalog_exports::checked_plan(&raw, &work.authority)?;
        work.job = "j".repeat(128);
        work.attempt = "a".repeat(128);
        f.binding = export_stage::Binding::from_work(work);
        f.begin.binding = f.binding.clone();
        let registration = register(&f);
        registration.validate()?;
        let binding = Binding {
            epoch: f.root.epoch.clone(),
            nonce: LeaseId::new(),
        };
        let packet = Packet {
            binding: binding.clone(),
            body: Body::Call {
                id: U64(1),
                call: Call::ExportNative(Box::new(registration.clone())),
            },
        };
        let encoded = encode(&packet, BYTES)?;
        assert!(
            encoded.len() > export_stage::REQUEST_BYTES,
            "test must exercise outer envelope beyond native request bound"
        );
        let Body::Call {
            call: Call::ExportNative(decoded),
            ..
        } = decode(&binding, &encoded, Lane::Data)?
        else {
            unreachable!()
        };
        let n::Action::Register { begin, .. } = &decoded.action else {
            unreachable!()
        };
        assert_eq!(begin.digest()?, f.begin.digest()?);
        let export_stage::Action::Begin { work, .. } = &begin.action else {
            unreachable!()
        };
        assert_eq!(work.plan.raw().len(), export_stage::PLAN_BYTES);
        assert_eq!(work.plan.raw().as_bytes(), raw.as_bytes());
        let pool = crate::preview::ByteBudget::new(f.worker)?;
        let stages = FakeStages::new(f._temp.path().to_owned());
        stages.pause_begin();
        let owner = Arc::new(Owner::new(
            f._temp.path().join("unused"),
            stages.clone(),
            1,
            &pool,
        )?);
        owner.bind(&f.root)?;
        owner.call(&decoded)?;
        let pending_begin = {
            let owner = owner.clone();
            let begin = begin.as_ref().clone();
            thread::spawn(move || owner.stage_call(&begin, &AtomicBool::new(false)))
        };
        stages.wait_ready_entered();
        let key = n::Key::new(&f.root, U64(9), &f.stage);
        let mut status = owner.query(&n::Query {
            key: key.clone(),
            action: n::QueryAction::Status,
        })?;
        assert_eq!(status.pending_stage_operation, Some(U64(1)));
        assert_eq!(status.pending_dispatch, n::DispatchState::Sent);
        status.error = Some("e".repeat(n::ERROR_BYTES));
        status.exit_code = Some(i32::MIN);
        status.pid = Some(u32::MAX);
        status.validate(&registration)?;
        let query = n::Query {
            key: key.clone(),
            action: n::QueryAction::Status,
        };
        let reply = Body::Control(Control::ExportNativeReply {
            id: U64(1),
            query: query.clone(),
            value: Ok(status.clone()),
        });
        let mut parent_output = Output::default();
        parent_output.push(&binding, reply.clone())?;
        let parent_replay = (1, query.clone(), reply.clone());
        let proxy = Proxy::new(binding.clone());
        {
            let mut state = proxy.state.lock().unwrap();
            state.export_native_query = Some(Pending {
                id: 1,
                query: query.clone(),
                active: true,
                result: None,
            });
            state.export_native_next = 2;
            state.output.push(
                &binding,
                Body::Control(Control::ExportNativeQuery {
                    id: U64(1),
                    query: query.clone(),
                }),
            )?;
            // Ordinary binary upload remains queued while reserved control is processed.
            let upload = export_stage::Request {
                root: f.root.clone(),
                stage: f.stage.clone(),
                operation: U64(2),
                supervisor: false,
                binding: f.binding.clone(),
                action: export_stage::Action::UploadIcc {
                    offset: U64(export_stage::BLOB_BYTES - export_stage::CHUNK_BYTES as u64),
                    bytes: vec![255; export_stage::CHUNK_BYTES],
                },
            };
            upload.validate()?;
            state.output.push(
                &binding,
                Body::Call {
                    id: U64(2),
                    call: Call::ExportStage(Box::new(upload)),
                },
            )?;
        }
        let emitted = parent_output
            .next(Lane::Control)
            .context("complete control reply")?;
        assert!(emitted.bytes.len() <= CONTROL_BYTES);
        proxy.receive(&emitted.bytes, Lane::Control)?;
        {
            let mut state = proxy.state.lock().unwrap();
            assert!(state.export_native_query.as_ref().unwrap().result.is_some());
            state.export_native_last = state.export_native_query.take();
            state.export_native_query = Some(Pending {
                id: 2,
                query: n::Query {
                    key,
                    action: n::QueryAction::Retire,
                },
                active: false,
                result: None,
            });
        }
        // Pending next query, prior consumed result, G replay and the emitted
        // packet coexist; replay doesn't consume the ordinary queued upload.
        parent_output.push(&binding, parent_replay.2.clone())?;
        let replay = parent_output.next(Lane::Control).unwrap();
        assert_eq!(replay.bytes.as_slice(), emitted.bytes.as_slice());
        proxy.receive(&replay.bytes, Lane::Control)?;
        let data = proxy
            .next(Lane::Data)
            .context("ordinary upload survives control replay")?;
        let Body::Call {
            call: Call::ExportStage(upload),
            ..
        } = decode(&binding, &data.bytes, Lane::Data)?
        else {
            unreachable!()
        };
        assert_eq!(upload.binary(), vec![255; export_stage::CHUNK_BYTES]);
        drop((
            packet,
            encoded,
            decoded,
            parent_replay,
            reply,
            replay,
            emitted,
            proxy,
        ));
        assert_eq!(
            pool.used(),
            f.worker,
            "dropping all relay graph copies cannot retire G custody"
        );
        stages.resume_begin();
        pending_begin.join().unwrap()?;
        owner.retire_root(&f.root)?;
        assert_eq!(pool.used(), 0);
        Ok(())
    }
}
