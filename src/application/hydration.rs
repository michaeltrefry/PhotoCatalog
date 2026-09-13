//! Initial source preparation for preview tickets. One hash reader per catalog;
//! foreground tickets preempt thumbnail preparation without discarding callers.
use super::*;
use crate::{catalog_edits::EditRenderIdentity, import_preparation::SinglePreparation};

struct Preparing {
    asset: String,
    path: NativePath,
    generation: i64,
    worker: SinglePreparation,
    canceling: bool,
}
struct Prepared {
    path: NativePath,
    generation: i64,
    fingerprint: String,
}
#[derive(Default)]
pub(super) struct State {
    preparing: Option<Preparing>,
    ready: HashMap<String, Prepared>,
}
fn waiting(t: &Ticket) -> bool {
    t.hydration && t.consumer.is_none() && matches!(t.dto.state, PreviewState::Queued)
}
fn post_hydration(
    expected: &EditRenderIdentity,
    current: &EditRenderIdentity,
    fingerprint: &str,
) -> bool {
    let mut post = expected.clone();
    post.source.state = "ready".into();
    post.source.fingerprint = Some(fingerprint.into());
    let Some(next) = post.source.generation.checked_add(1) else {
        return false;
    };
    post.source.generation = next;
    let Some(image) = &mut post.image_identity else {
        return false;
    };
    let Some(next) = image.physical_generation.checked_add(1) else {
        return false;
    };
    image.physical_generation = next;
    identity_equal(&post, current)
}
impl State {
    pub(super) fn request_cancel(&mut self) {
        if let Some(preparing) = &mut self.preparing {
            preparing.canceling = true;
            preparing.worker.request_cancel();
        }
    }
    pub(super) fn completed(
        &self,
        catalog: &Catalog,
        ticket: &mut Ticket,
        done: &preview::ServiceCompletion,
    ) -> bool {
        if !matches!(
            done,
            preview::ServiceCompletion::Ready | preview::ServiceCompletion::Stale
        ) {
            return false;
        }
        let Some(prepared) = self.ready.get(&ticket.dto.key.asset_id) else {
            return false;
        };
        let Ok(current) = catalog.edit_render_identity(&ticket.dto.key) else {
            return false;
        };
        if !post_hydration(&ticket.identity, &current, &prepared.fingerprint)
            || catalog
                .preview_original_path(&ticket.dto.key.asset_id)
                .ok()
                .as_ref()
                != Some(&prepared.path)
        {
            return false;
        }
        ticket.identity = current;
        ticket.hydration = false;
        // A sibling may have hydrated the physical source first. Its selected
        // recipe is never substituted for this ticket; request this variant next.
        ticket.dto.state = if matches!(done, preview::ServiceCompletion::Ready) {
            PreviewState::Ready
        } else {
            PreviewState::Queued
        };
        ticket.dto.message = None;
        true
    }
    pub(super) fn advance(
        &mut self,
        catalog: &mut Catalog,
        service: &mut PreviewService,
        tickets: &mut HashMap<String, Ticket>,
        #[cfg(test)] checkpoint: Option<crate::import_preparation::Checkpoint>,
    ) {
        self.ready.retain(|asset, _| {
            tickets
                .values()
                .any(|t| t.hydration && t.dto.key.asset_id == *asset)
        });
        if let Some(preparing) = &mut self.preparing {
            let relevant = tickets
                .values()
                .any(|t| waiting(t) && t.dto.key.asset_id == preparing.asset);
            let foreground_here = tickets
                .values()
                .any(|t| waiting(t) && t.foreground && t.dto.key.asset_id == preparing.asset);
            let foreground_elsewhere = tickets
                .values()
                .any(|t| waiting(t) && t.foreground && t.dto.key.asset_id != preparing.asset);
            if !relevant || (!foreground_here && foreground_elsewhere) {
                preparing.canceling = true;
                preparing.worker.request_cancel();
            }
            if preparing.canceling && preparing.worker.is_finished() {
                self.preparing = None;
            }
        }
        if let Some(preparing) = &mut self.preparing
            && !preparing.canceling
        {
            let result = match preparing.worker.poll() {
                Ok(None) => None,
                Ok(Some(result)) => Some(result),
                Err(e) => Some(Err(format!("{e:#}"))),
            };
            if let Some(result) = result {
                let preparing = self.preparing.take().unwrap();
                match result {
                    Ok(fingerprint) => {
                        self.ready.insert(
                            preparing.asset,
                            Prepared {
                                path: preparing.path,
                                generation: preparing.generation,
                                fingerprint,
                            },
                        );
                    }
                    Err(message) => {
                        for t in tickets
                            .values_mut()
                            .filter(|t| waiting(t) && t.dto.key.asset_id == preparing.asset)
                        {
                            t.hydration = false;
                            t.dto.state = PreviewState::Unavailable;
                            t.dto.message = Some(message.chars().take(2048).collect());
                        }
                    }
                }
            }
        }
        let mut candidates: Vec<_> = tickets
            .iter()
            .filter(|(_, t)| t.consumer.is_none() && matches!(t.dto.state, PreviewState::Queued))
            .map(|(id, t)| (id.clone(), !t.foreground, t.touched))
            .collect();
        candidates.sort_by_key(|(_, background, at)| (*background, *at));
        for (id, _, _) in candidates.into_iter().take(8) {
            let t = tickets.get_mut(&id).unwrap();
            let result = (|| -> Result<()> {
                let current = catalog.edit_render_identity(&t.dto.key)?;
                let path = catalog.preview_original_path(&t.dto.key.asset_id)?;
                if t.hydration && current.source.state == "ready" {
                    let valid = self.ready.get(&t.dto.key.asset_id).is_some_and(|p| {
                        p.path == path && post_hydration(&t.identity, &current, &p.fingerprint)
                    });
                    ensure!(
                        valid,
                        "original or recipe changed during preparation; request preview again"
                    );
                    t.identity = current.clone();
                    t.hydration = false;
                }
                ensure!(
                    identity_equal(&t.identity, &current),
                    "recipe or source changed during preparation; request preview again"
                );
                let priority = if t.foreground {
                    preview::Priority::Foreground
                } else {
                    preview::Priority::Background
                };
                if t.hydration {
                    ensure!(
                        crate::initial_hydration_source(&catalog.db, &t.dto.key.asset_id, &path)?,
                        "initial original identity changed"
                    );
                    service.ensure_original_separate(&path.to_path()?)?;
                    if let Some(prepared) = self.ready.get(&t.dto.key.asset_id) {
                        ensure!(
                            prepared.path == path
                                && prepared.generation == current.source.generation,
                            "original path changed during preparation"
                        );
                        if service.available_request_slots() > 0 {
                            t.consumer = Some(service.submit_hydration(
                                catalog,
                                preview::HydrationRequest {
                                    variant: &t.dto.key,
                                    source: &path,
                                    fingerprint: &prepared.fingerprint,
                                    tier: t.tier,
                                    priority,
                                    interactive: t.interactive,
                                },
                            )?);
                            t.dto.message = Some("rendering original".into());
                        }
                    } else if self.preparing.is_none() {
                        let worker = SinglePreparation::spawn(
                            path.to_path()?,
                            #[cfg(test)]
                            checkpoint.clone(),
                        )?;
                        self.preparing = Some(Preparing {
                            asset: t.dto.key.asset_id.clone(),
                            path,
                            generation: current.source.generation,
                            worker,
                            canceling: false,
                        });
                    }
                } else {
                    let cached = if t.interactive {
                        service.cached_interactive(catalog, &t.dto.key, t.tier, false)?
                    } else {
                        service.cached_variant(catalog, &t.dto.key, t.tier, false)?
                    };
                    if cached.is_some() {
                        t.dto.state = PreviewState::Ready;
                    } else if service.available_request_slots() > 0 {
                        t.consumer = Some(if t.interactive {
                            service.request_interactive(catalog, &t.dto.key, t.tier, priority)?
                        } else {
                            service.request_variant(catalog, &t.dto.key, t.tier, priority)?
                        });
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                t.hydration = false;
                t.dto.state = PreviewState::Stale;
                t.dto.message = Some(format!("{e:#}").chars().take(2048).collect());
            }
        }
    }
}
