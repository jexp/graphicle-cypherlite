use super::*;

impl Graph {
    pub fn apply_redo(&self, ops: Vec<RedoOp>, commit_ts: u64) -> Result<()> {
        self.apply_redo_inner(ops, commit_ts, false)
    }

    /// Replay historical WAL under the old admission policy, preserving unknown
    /// enforcement as legacy rather than inventing a typed checkpoint image.
    #[doc(hidden)]
    pub fn apply_replayed_redo(&self, ops: Vec<RedoOp>, commit_ts: u64) -> Result<()> {
        self.apply_redo_inner(ops, commit_ts, true)
    }

    pub(super) fn apply_redo_inner(
        &self,
        ops: Vec<RedoOp>,
        commit_ts: u64,
        legacy_replay: bool,
    ) -> Result<()> {
        self.validate_redo_group(std::iter::once((ops.as_slice(), commit_ts)), !legacy_replay)?;
        let legacy_created = if legacy_replay {
            ops.iter()
                .any(|op| matches!(op, RedoOp::CreateConstraint { .. }))
        } else {
            false
        };
        self.in_write.store(true, Ordering::Release);
        self.write_ts.store(commit_ts, Ordering::Release);
        self.record_redo.store(false, Ordering::Release);
        for op in ops {
            match op {
                RedoOp::CreateNode {
                    id,
                    labels,
                    properties,
                } => {
                    // Move the typed fields in — the caller consumed the WAL
                    // borrow before apply, so no clone is needed (ADR-0078).
                    self.install_node(
                        id,
                        std::sync::Arc::unwrap_or_clone(labels),
                        std::sync::Arc::unwrap_or_clone(properties).into(),
                    )?;
                }
                RedoOp::CreateRel {
                    id,
                    start,
                    end,
                    rel_type,
                    properties,
                } => {
                    self.install_rel(
                        id,
                        start,
                        end,
                        rel_type,
                        std::sync::Arc::unwrap_or_clone(properties).into(),
                    )?;
                }
                RedoOp::DeleteNode { id } => {
                    self.delete_node(id, true)?;
                    self.drop_pull_identity(PullKind::Node, id);
                }
                RedoOp::DeleteRel { id } => {
                    self.delete_rel(id);
                    self.drop_pull_identity(PullKind::Rel, id);
                }
                RedoOp::SetNodeProp { id, key, value } => {
                    self.set_property_node(id, &key, value);
                }
                RedoOp::RemoveNodeProp { id, key } => {
                    self.set_property_node(id, &key, Value::Null);
                }
                RedoOp::SetRelProp { id, key, value } => {
                    self.set_property_rel(id, &key, value);
                }
                RedoOp::RemoveRelProp { id, key } => {
                    self.set_property_rel(id, &key, Value::Null);
                }
                RedoOp::AddLabels { id, labels } => {
                    self.add_labels(id, &labels);
                }
                RedoOp::RemoveLabels { id, labels } => {
                    self.remove_labels(id, &labels);
                }
                RedoOp::CreateIndex {
                    name,
                    label,
                    property,
                } => {
                    self.install_named_index(IndexDef {
                        name,
                        label,
                        property,
                    })?;
                }
                RedoOp::DropIndex { name } => {
                    self.catalog.lock().unwrap().drop_index(&name);
                    let removed = {
                        let mut theater = self.theater.lock().unwrap();
                        theater.index_options.remove(&name);
                        theater.index_details.remove(&name)
                    };
                    if let Some(detail) = removed {
                        self.drop_detail(&detail);
                        self.refresh_index_charge();
                        self.recompute_used();
                    }
                }
                RedoOp::CreateConstraint {
                    name,
                    relationship,
                    label,
                    properties,
                    kind,
                    property_type,
                } => {
                    self.catalog
                        .lock()
                        .unwrap()
                        .insert_constraint(ConstraintDef {
                            name,
                            relationship,
                            label,
                            properties,
                            kind,
                            property_type,
                        })?;
                }
                RedoOp::DropConstraint { name } => {
                    self.catalog.lock().unwrap().drop_constraint(&name);
                    self.theater
                        .lock()
                        .unwrap()
                        .derived_constraints
                        .remove(&name);
                    self.legacy_unenforced.lock().unwrap().remove(&name);
                }
                RedoOp::CatalogDerivedConstraint { constraint } => {
                    if let Some(enforced) = &constraint.enforced_label {
                        // An enforced label is always a node label (even on a
                        // relationship constraint, `sourceLabel`/`targetLabel`
                        // name the endpoint's label).
                        self.record_schema_names(false, std::iter::once(enforced.clone()));
                    }
                    if self
                        .catalog
                        .lock()
                        .unwrap()
                        .constraints
                        .get(&constraint.name)
                        .is_some_and(|def| matching_enforced_label(def, &constraint))
                    {
                        self.legacy_unenforced
                            .lock()
                            .unwrap()
                            .remove(&constraint.name);
                    }
                    self.theater
                        .lock()
                        .unwrap()
                        .derived_constraints
                        .insert(constraint.name.clone(), constraint);
                }
                RedoOp::CatalogIndexDetail {
                    name,
                    detail,
                    options,
                } => {
                    // Decode before mutating: a corrupted op is an error,
                    // never a silently dropped index detail/options entry.
                    let options = redo::props_from_json(&options)?;
                    // The typed detail is the only op that knows whether its
                    // labels are node labels or relationship types; the
                    // catalog's `CreateIndex` row is entity-kind-blind.
                    self.record_schema_names(detail.relationship, detail.labels.iter().cloned());
                    {
                        let mut theater = self.theater.lock().unwrap();
                        theater.index_details.insert(name.clone(), detail.clone());
                        theater.index_options.insert(name, options);
                    }
                    self.refresh_index_charge();
                    self.materialize_detail(&detail);
                    self.recompute_used();
                }
                RedoOp::PutDatabase {
                    name,
                    language,
                    composite,
                } => {
                    // ADR-0276: the databases catalog is store-level,
                    // shared by every namespace graph; per the ADR-0116 §8
                    // invariants only the fixed namespaces are kept,
                    // replayed entries for other names are ignored.
                    self.databases().put_database(&name, language, composite);
                }
                RedoOp::DropDatabase { .. } => {
                    // ADR-0116 §1/§8: the fixed namespaces cannot be
                    // dropped, and the `Namespace`-keyed catalog can only
                    // ever hold them, so a legacy `DROP DATABASE` redo is
                    // a no-op.
                }
                RedoOp::PutGraphType { name, body } => {
                    self.theater.lock().unwrap().graph_types.insert(name, body);
                }
                RedoOp::SetCurrentGraphType { body } => {
                    let mut theater = self.theater.lock().unwrap();
                    theater.set_current_graph_type(&body)?;
                    let derived = theater.derived_constraints.clone();
                    drop(theater);
                    let catalog = self.catalog.lock().unwrap();
                    self.legacy_unenforced.lock().unwrap().retain(|name, _| {
                        !catalog.constraints.get(name).is_some_and(|def| {
                            derived
                                .get(name)
                                .is_some_and(|constraint| matching_enforced_label(def, constraint))
                        })
                    });
                }
                RedoOp::PullIdentityPut {
                    source,
                    kind,
                    remote_id,
                    local_id,
                } => {
                    self.pull_identity_put(source, kind, remote_id, local_id);
                }
                RedoOp::OntologySet { .. } | RedoOp::OntologyDrop { .. } => {
                    // ADR-0120 §2/§3: store-level ops; they carry no graph
                    // mutation. The document lives with the store layer —
                    // no graph is derived from it (ADR-0229 §3) — so replay
                    // is a deliberate no-op here.
                }
            }
        }
        self.pending_redo.lock().unwrap().clear();
        self.set_commit_ts(commit_ts);
        self.in_write.store(false, Ordering::Release);
        self.record_redo.store(true, Ordering::Release);
        if legacy_created {
            self.mark_legacy_unenforced();
        }
        Ok(())
    }
}
