//! Application-route reconciliation and prepared-chain activation.
//!
//! These methods remain inherent on WindowsAudioDriver so callers and the
//! public API keep their existing shape. Keeping them in a child module keeps
//! route policy and effect restoration out of the driver composition file.

use super::*;

impl WindowsAudioDriver {
    pub fn reconcile_application_routes(
        &mut self,
        routes: Vec<pw_graph_config::WindowsApplicationRoute>,
    ) -> BackendResult<Vec<ApplicationRoutePlan>> {
        self.application_routes.set_rules(routes);
        // Refreshing after installing the rules lets the same refresh pass
        // resolve the current PID, request its capture lease, and publish the
        // final state instead of returning an artificial intermediate plan.
        self.refresh_snapshot(true)?;
        Ok(self.application_routes.plans().cloned().collect())
    }

    fn cancel_pending_application_route(&mut self, rule_index: usize) {
        let tickets: Vec<_> = self
            .pending_application_effects
            .iter()
            .filter_map(|(ticket, pending)| (pending.rule_index == rule_index).then_some(*ticket))
            .collect();
        for ticket in tickets {
            let _ = self.effects.cancel_effect(ticket);
            self.pending_application_effects.remove(&ticket);
        }
        self.pending_application_routes.remove(&rule_index);
    }

    pub(super) fn clear_application_route_links(&mut self) -> BackendResult<()> {
        let mut rules = BTreeSet::new();
        rules.extend(self.application_route_links.keys().copied());
        rules.extend(self.application_route_effects.keys().copied());
        rules.extend(self.application_route_activations.keys().copied());
        rules.extend(self.pending_application_routes.keys().copied());
        for rule in rules {
            self.remove_application_route(rule)?;
        }
        Ok(())
    }

    fn remove_application_route(&mut self, rule_index: usize) -> BackendResult<()> {
        self.cancel_pending_application_route(rule_index);
        let links = self
            .application_route_links
            .remove(&rule_index)
            .unwrap_or_default();
        let mut first_error = None;
        for link_id in links {
            if let Some(routing) = self.routing.as_mut() {
                if routing.owns(link_id) {
                    match routing.disconnect(link_id) {
                        Ok(removed) => {
                            let _ = self.graph.remove_link(removed.id);
                        }
                        Err(error) => {
                            first_error.get_or_insert(error);
                        }
                    }
                } else {
                    let _ = self.graph.remove_link(link_id);
                }
            } else {
                let _ = self.graph.remove_link(link_id);
            }
        }

        let effect_ids = self
            .application_route_effects
            .remove(&rule_index)
            .unwrap_or_default();
        for effect_id in effect_ids.into_iter().rev() {
            if let Err(error) = self.destroy_effect(&effect_id) {
                first_error.get_or_insert(error);
            }
        }
        self.application_route_activations.remove(&rule_index);
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    /// Apply the safe, direct portion of an active persisted route. The
    /// reconciler performs identity/isolation/capture checks; this is the
    /// transactional graph/router boundary that turns an accepted plan into
    /// an actual process-loopback -> endpoint route.
    ///
    /// Effect instances and their links are created as one route transaction.
    /// A route must never be reported active while silently bypassing a saved
    /// processor or while leaving only half of its graph chain installed.
    fn apply_application_route_plans(&mut self) -> BackendResult<()> {
        let mut desired: BTreeMap<usize, (ApplicationRouteActivation, PortId, PortId)> =
            BTreeMap::new();
        let mut unapplied = Vec::new();
        for plan in self.application_routes.plans() {
            let Some(activation) = plan.activation.as_ref() else {
                continue;
            };
            let Some(selector) = activation.selector.runtime_key() else {
                unapplied.push((
                    plan.rule_index,
                    "active route has no runtime application identity".into(),
                ));
                continue;
            };
            let Some(source) = self
                .application_route_ports
                .get(&(selector, activation.pid))
                .copied()
            else {
                unapplied.push((
                    plan.rule_index,
                    "isolated application source port is no longer present".into(),
                ));
                continue;
            };
            let Some(destination_id) = activation.destination.current_mmdevice_id.as_deref() else {
                unapplied.push((
                    plan.rule_index,
                    "resolved destination has no current MMDevice id".into(),
                ));
                continue;
            };
            let Some(destination) = self
                .endpoint_ports
                .iter()
                .find(|(_, endpoint)| {
                    endpoint.role == EndpointPortRole::Render
                        && endpoint.device_id == destination_id
                })
                .map(|(port, _)| *port)
            else {
                unapplied.push((
                    plan.rule_index,
                    "resolved destination render port is no longer present".into(),
                ));
                continue;
            };
            desired.insert(plan.rule_index, (activation.clone(), source, destination));
        }
        for (rule, reason) in unapplied {
            self.application_routes.mark_degraded(rule, reason);
        }

        let existing: BTreeSet<_> = self
            .application_route_activations
            .keys()
            .chain(self.application_route_links.keys())
            .chain(self.application_route_effects.keys())
            .chain(self.pending_application_routes.keys())
            .copied()
            .collect();
        for rule in existing {
            let keep = desired
                .get(&rule)
                .is_some_and(|(activation, source, destination)| {
                    let active = self.application_route_activations.get(&rule) == Some(activation)
                        && self
                            .application_route_effects
                            .get(&rule)
                            .and_then(|effect_ids| {
                                self.route_link_ids(*source, *destination, effect_ids)
                            })
                            .is_some_and(|link_ids| {
                                self.application_route_links.get(&rule) == Some(&link_ids)
                                    && link_ids.iter().all(|link| {
                                        self.routing
                                            .as_ref()
                                            .is_some_and(|routing| routing.owns(*link))
                                    })
                            });
                    let preparing =
                        self.pending_application_routes
                            .get(&rule)
                            .is_some_and(|pending| {
                                pending.activation == *activation
                                    && pending.source == *source
                                    && pending.destination == *destination
                            });
                    active || preparing
                });
            if !keep {
                self.remove_application_route(rule)?;
            }
        }

        for plan in self.application_routes.plans().cloned().collect::<Vec<_>>() {
            let Some((activation, source, destination)) = desired.get(&plan.rule_index).cloned()
            else {
                continue;
            };
            if self.application_route_activations.get(&plan.rule_index) == Some(&activation) {
                continue;
            }
            if self
                .pending_application_routes
                .get(&plan.rule_index)
                .is_some_and(|pending| {
                    pending.activation == activation
                        && pending.source == source
                        && pending.destination == destination
                })
            {
                continue;
            }
            if activation.effect_instances.is_empty() {
                if self.routing.is_none() {
                    self.routing = Some(WindowsRouting::start()?);
                }
                match self.install_application_route_prepared(
                    plan.rule_index,
                    activation.clone(),
                    source,
                    destination,
                    Vec::new(),
                ) {
                    Ok((links, effects)) => {
                        self.application_route_links.insert(plan.rule_index, links);
                        self.application_route_effects
                            .insert(plan.rule_index, effects);
                        self.application_route_activations
                            .insert(plan.rule_index, activation);
                    }
                    Err(error) => {
                        self.application_routes.mark_degraded(
                            plan.rule_index,
                            format!("could not activate restored route: {error}"),
                        );
                    }
                }
            } else if let Err(error) = self.begin_application_route_preparation(
                plan.rule_index,
                activation,
                source,
                destination,
            ) {
                self.application_routes.mark_effect_restore_failed(
                    plan.rule_index,
                    format!("could not prepare restored effect chain: {error}"),
                );
            }
        }
        Ok(())
    }

    fn begin_application_route_preparation(
        &mut self,
        rule_index: usize,
        activation: ApplicationRouteActivation,
        source: PortId,
        destination: PortId,
    ) -> BackendResult<()> {
        if activation.effect_instances.len() > 16 {
            return Err(BackendError::unsupported(
                "application effect chain exceeds the Windows route limit of 16 processors",
            ));
        }
        let spec = WindowsRouting::effect_spec(WindowsRouting::block_frames());
        let mut tickets = Vec::with_capacity(activation.effect_instances.len());
        for (effect_index, config) in activation.effect_instances.iter().enumerate() {
            let mut route_config = config.clone();
            route_config.instance_id =
                Self::route_effect_id(rule_index, effect_index, &config.instance_id);
            let ticket = match self.effects.begin_prepare_config(&route_config, spec) {
                Ok(ticket) => ticket,
                Err(error) => {
                    for (_, ticket) in tickets {
                        let _ = self.effects.cancel_effect(ticket);
                    }
                    return Err(error);
                }
            };
            tickets.push((effect_index, ticket));
        }
        for (effect_index, ticket) in tickets {
            self.pending_application_effects.insert(
                ticket,
                PendingApplicationEffect {
                    rule_index,
                    effect_index,
                },
            );
        }
        self.pending_application_routes.insert(
            rule_index,
            PendingApplicationRoute {
                activation,
                source,
                destination,
                prepared: BTreeMap::new(),
            },
        );
        self.application_routes
            .mark_effects_preparing(rule_index, "loading the saved effect chain".into());
        Ok(())
    }

    pub(super) fn handle_application_effect_ready(
        &mut self,
        ticket: EffectTicket,
        prepared: pw_graph_effects::PreparedEffect,
    ) -> BackendResult<()> {
        let Some(pending_effect) = self.pending_application_effects.remove(&ticket) else {
            // The route was cancelled or replaced while the loader was
            // finishing. Dropping the prepared processor is intentional.
            return Ok(());
        };
        let Some(route) = self
            .pending_application_routes
            .get_mut(&pending_effect.rule_index)
        else {
            return Ok(());
        };
        route.prepared.insert(pending_effect.effect_index, prepared);
        if route.prepared.len() != route.activation.effect_instances.len() {
            return Ok(());
        }
        let route = self
            .pending_application_routes
            .remove(&pending_effect.rule_index)
            .expect("pending route was just inspected");
        let PendingApplicationRoute {
            activation,
            source,
            destination,
            prepared,
        } = route;
        let prepared = prepared.into_values().collect();
        match self.install_application_route_prepared(
            pending_effect.rule_index,
            activation.clone(),
            source,
            destination,
            prepared,
        ) {
            Ok((links, effects)) => {
                self.application_route_links
                    .insert(pending_effect.rule_index, links);
                self.application_route_effects
                    .insert(pending_effect.rule_index, effects);
                self.application_route_activations
                    .insert(pending_effect.rule_index, activation);
                self.application_routes
                    .mark_effects_active(pending_effect.rule_index);
                self.dirty.store(true, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                self.application_routes.mark_effect_restore_failed(
                    pending_effect.rule_index,
                    format!("could not activate restored effect chain: {error}"),
                );
                Ok(())
            }
        }
    }

    pub(super) fn handle_application_effect_failure(
        &mut self,
        ticket: EffectTicket,
        reason: String,
    ) {
        let Some(pending_effect) = self.pending_application_effects.remove(&ticket) else {
            return;
        };
        let rule_index = pending_effect.rule_index;
        self.cancel_pending_application_route(rule_index);
        self.application_routes
            .mark_effect_restore_failed(rule_index, reason);
    }

    fn route_effect_id(rule_index: usize, position: usize, instance_id: &str) -> String {
        format!("windows-application-route:{rule_index}:{position}:{instance_id}")
    }

    fn route_link_ids(
        &self,
        source: PortId,
        destination: PortId,
        effect_ids: &[String],
    ) -> Option<Vec<LinkId>> {
        let mut output = source;
        let mut links = Vec::with_capacity(effect_ids.len() + 1);
        for effect_id in effect_ids {
            let effect = self.effects.get(effect_id)?;
            links.push(managed_link(output, effect.input_port).id);
            output = effect.output_port;
        }
        links.push(managed_link(output, destination).id);
        Some(links)
    }

    fn route_effect_position(
        &self,
        source: PortId,
        destination: PortId,
        index: usize,
        count: usize,
    ) -> [f32; 2] {
        let source_position = self
            .graph
            .port(source)
            .and_then(|port| self.graph.node(port.node_id))
            .map(|node| node.position)
            .unwrap_or([0.0, 0.0]);
        let destination_position = self
            .graph
            .port(destination)
            .and_then(|port| self.graph.node(port.node_id))
            .map(|node| node.position)
            .unwrap_or([320.0, 0.0]);
        let fraction = (index + 1) as f32 / (count + 1) as f32;
        [
            source_position[0] + (destination_position[0] - source_position[0]) * fraction,
            source_position[1] + (destination_position[1] - source_position[1]) * fraction,
        ]
    }

    fn install_application_route_prepared(
        &mut self,
        rule_index: usize,
        activation: ApplicationRouteActivation,
        source: PortId,
        destination: PortId,
        prepared: Vec<pw_graph_effects::PreparedEffect>,
    ) -> BackendResult<(Vec<LinkId>, Vec<String>)> {
        if activation.effect_instances.len() > 16 {
            return Err(BackendError::unsupported(
                "application effect chain exceeds the Windows route limit of 16 processors",
            ));
        }
        if prepared.len() != activation.effect_instances.len() {
            return Err(BackendError::native(format!(
                "prepared {} processors for an effect chain that requires {}",
                prepared.len(),
                activation.effect_instances.len()
            )));
        }
        let mut effect_ids = Vec::with_capacity(activation.effect_instances.len());
        for ((index, config), prepared) in
            activation.effect_instances.iter().enumerate().zip(prepared)
        {
            let mut route_config = config.clone();
            route_config.instance_id =
                Self::route_effect_id(rule_index, index, &config.instance_id);
            let position = self.route_effect_position(
                source,
                destination,
                index,
                activation.effect_instances.len(),
            );
            match self.create_application_effect_prepared(route_config, position, prepared) {
                Ok(_) => effect_ids.push(Self::route_effect_id(
                    rule_index,
                    index,
                    &config.instance_id,
                )),
                Err(error) => {
                    for effect_id in effect_ids.into_iter().rev() {
                        let _ = self.destroy_effect(&effect_id);
                    }
                    return Err(error);
                }
            }
        }

        let Some(link_ids) = self.route_link_ids(source, destination, &effect_ids) else {
            for effect_id in effect_ids.into_iter().rev() {
                let _ = self.destroy_effect(&effect_id);
            }
            return Err(BackendError::native(
                "restored application effect disappeared",
            ));
        };
        let links: Vec<_> = {
            let mut output = source;
            let mut links = Vec::with_capacity(effect_ids.len() + 1);
            for effect_id in &effect_ids {
                let effect = self
                    .effects
                    .get(effect_id)
                    .expect("effect was just created");
                links.push(managed_link(output, effect.input_port));
                output = effect.output_port;
            }
            links.push(managed_link(output, destination));
            links
        };
        let mut connected = Vec::new();
        for link in &links {
            let result = self
                .routing
                .as_mut()
                .expect("routing was started before installing a route")
                .connect(link.clone(), &self.endpoint_ports);
            if let Err(error) = result {
                for link_id in connected.into_iter().rev() {
                    if let Some(routing) = self.routing.as_mut() {
                        let _ = routing.disconnect(link_id);
                    }
                    let _ = self.graph.remove_link(link_id);
                }
                for effect_id in effect_ids.into_iter().rev() {
                    let _ = self.destroy_effect(&effect_id);
                }
                return Err(error);
            }
            connected.push(link.id);
        }

        let gain_result = self
            .routing
            .as_mut()
            .expect("routing is still present")
            .set_source_gain(source, activation.gain.clamp(0.0, 1.5));
        if let Err(error) = gain_result {
            for link_id in connected.iter().rev().copied() {
                if let Some(routing) = self.routing.as_mut() {
                    let _ = routing.disconnect(link_id);
                }
                let _ = self.graph.remove_link(link_id);
            }
            for effect_id in effect_ids.into_iter().rev() {
                let _ = self.destroy_effect(&effect_id);
            }
            return Err(error);
        }

        let mut graph_links = Vec::new();
        for link in &links {
            if let Err(error) = self
                .graph
                .add_link(link.id, link.output_port, link.input_port)
            {
                for link_id in graph_links.into_iter().rev() {
                    let _ = self.graph.remove_link(link_id);
                }
                for link_id in connected.into_iter().rev() {
                    if let Some(routing) = self.routing.as_mut() {
                        let _ = routing.disconnect(link_id);
                    }
                }
                for effect_id in effect_ids.into_iter().rev() {
                    let _ = self.destroy_effect(&effect_id);
                }
                return Err(error.into());
            }
            graph_links.push(link.id);
        }
        Ok((link_ids, effect_ids))
    }

    const AUTOMATIC_APP_ROUTE_ROLES: [AudioRole; 3] = [
        AudioRole::Console,
        AudioRole::Multimedia,
        AudioRole::Communications,
    ];

    fn app_render_endpoint_selector(&self) -> Option<WindowsEndpointSelector> {
        let identities: Vec<_> = self
            .virtual_endpoint_identities
            .iter()
            .filter(|identity| identity.role == QpwVirtualEndpointRole::AppRender)
            .collect();
        let [identity] = identities.as_slice() else {
            return None;
        };
        self.endpoint_selectors
            .get(&identity.mmdevice_id)
            .filter(|selector| selector.data_flow == AudioFlow::Render)
            .cloned()
    }

    fn unique_application_candidate(
        &self,
        selector: &pw_graph_config::WindowsApplicationSelector,
    ) -> Option<ApplicationRouteCandidate> {
        let mut matches = self
            .application_route_candidates
            .iter()
            .filter(|candidate| selector.matches(&candidate.selector));
        let candidate = matches.next()?.clone();
        matches.next().is_none().then_some(candidate)
    }

    fn route_rule_is_winner(
        &self,
        rule_index: usize,
        selector: &pw_graph_config::WindowsApplicationSelector,
    ) -> bool {
        self.application_routes
            .rules()
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.matches_application(selector))
            .min_by(|(left_index, left), (right_index, right)| {
                right
                    .selector_specificity()
                    .cmp(&left.selector_specificity())
                    .then_with(|| left_index.cmp(right_index))
            })
            .is_some_and(|(winner, _)| winner == rule_index)
    }

    fn selector_for_policy_endpoint(
        &self,
        endpoint: Option<&str>,
    ) -> Option<WindowsEndpointSelector> {
        let endpoint = endpoint.filter(|endpoint| !endpoint.trim().is_empty())?;
        if let Some(selector) = self.endpoint_selectors.get(endpoint) {
            return (selector.data_flow == AudioFlow::Render).then_some(selector.clone());
        }
        // The saved endpoint may be temporarily absent. Preserve its exact
        // MMDevice id for a later restore; never downgrade it to a friendly
        // name or choose a replacement endpoint.
        Some(WindowsEndpointSelector {
            stable_id: None,
            current_mmdevice_id: Some(endpoint.to_owned()),
            friendly_name: None,
            data_flow: AudioFlow::Render,
        })
    }

    fn endpoint_id_matches(selector: &WindowsEndpointSelector, endpoint: Option<&str>) -> bool {
        selector
            .current_mmdevice_id
            .as_deref()
            .zip(endpoint)
            .is_some_and(|(expected, actual)| expected.eq_ignore_ascii_case(actual))
    }

    pub(super) fn restore_automatic_application_route_policies(&mut self, force: bool) {
        let lease_keys: Vec<_> = self.application_route_leases.keys().copied().collect();
        let virtual_output = self.app_render_endpoint_selector();
        for (rule_index, role) in lease_keys {
            let Some(lease) = self
                .application_route_leases
                .get(&(rule_index, role))
                .cloned()
            else {
                continue;
            };
            let Some(candidate) = self.unique_application_candidate(&lease.selector) else {
                // A stopped process has no safe PID to call. Keep the lease
                // until the same stable selector returns with a new PID.
                continue;
            };
            let Ok(identity) = ProcessIdentity::from_pid(candidate.pid) else {
                continue;
            };
            if !lease.selector.matches(&identity.application_selector()) {
                continue;
            }
            let still_requested = !force
                && self
                    .application_routes
                    .rules()
                    .get(rule_index)
                    .is_some_and(|rule| {
                        rule.enabled
                            && self.route_rule_is_winner(rule_index, &lease.selector)
                            && virtual_output.is_some()
                    });
            if lease.process_id != candidate.pid {
                // The private policy is keyed by PID on the verified ABI. A
                // selector-matched replacement process therefore needs the
                // old lease's original endpoint, but it must not be treated
                // as a user override merely because its new PID has no
                // persisted value yet. The automatic sync below reapplies
                // the target to this PID. If the rule was removed, the old
                // PID is already gone and there is no owned value on the new
                // PID to restore.
                if !still_requested {
                    self.application_route_leases.remove(&(rule_index, role));
                }
                continue;
            }
            if !lease.owned {
                if !still_requested {
                    self.application_route_leases.remove(&(rule_index, role));
                }
                continue;
            }
            let Ok(current) =
                self.app_route_policy
                    .get_persisted_endpoint(&identity, AudioFlow::Render, role)
            else {
                continue;
            };
            if !Self::endpoint_id_matches(&lease.applied_endpoint, current.as_deref()) {
                // A current endpoint other than the one qpwgraph applied is a
                // user or third-party override. Drop ownership and preserve
                // that choice; do not restore over it later.
                if still_requested {
                    if let Some(lease) = self.application_route_leases.get_mut(&(rule_index, role))
                    {
                        lease.mark_user_override();
                    }
                } else {
                    self.application_route_leases.remove(&(rule_index, role));
                }
                continue;
            }
            if still_requested {
                continue;
            }
            let original = lease
                .original_endpoint
                .as_ref()
                .and_then(|endpoint| endpoint.current_mmdevice_id.as_deref());
            let restored = self
                .app_route_policy
                .set_persisted_endpoint(&identity, AudioFlow::Render, role, original)
                .is_ok()
                && self
                    .app_route_policy
                    .get_persisted_endpoint(&identity, AudioFlow::Render, role)
                    .is_ok_and(|current| match lease.original_endpoint.as_ref() {
                        Some(original) => Self::endpoint_id_matches(original, current.as_deref()),
                        None => current.is_none(),
                    });
            if restored {
                self.application_route_leases.remove(&(rule_index, role));
            }
        }
    }

    /// Apply or reconcile the private per-application endpoint policy. This
    /// method never changes graph state itself: a Core Audio refresh must first
    /// prove that the application session actually moved to AppRender before
    /// the ordinary reconciler can request capture/effects.
    fn sync_automatic_application_route_policies(&mut self) -> bool {
        self.restore_automatic_application_route_policies(false);
        if !matches!(
            self.app_route_policy.support(),
            AppRoutePolicySupport::Experimental { .. }
        ) {
            return false;
        }
        let Some(virtual_output) = self.app_render_endpoint_selector() else {
            return false;
        };
        let rules = self.application_routes.rules().to_vec();
        let mut changed = false;
        for (rule_index, rule) in rules.iter().enumerate() {
            if !rule.enabled || !self.route_rule_is_winner(rule_index, &rule.application) {
                continue;
            }
            let Some(candidate) = self.unique_application_candidate(&rule.application) else {
                continue;
            };
            if candidate.isolated || candidate.pid == 0 {
                continue;
            }
            let Ok(identity) = ProcessIdentity::from_pid(candidate.pid) else {
                continue;
            };
            let live_selector = identity.application_selector();
            if !rule.application.matches(&live_selector)
                || !candidate.selector.matches(&live_selector)
            {
                continue;
            }

            let existing_leases: Vec<_> = Self::AUTOMATIC_APP_ROUTE_ROLES
                .iter()
                .map(|role| {
                    self.application_route_leases
                        .get(&(rule_index, *role))
                        .cloned()
                })
                .collect();
            let existing_lease = existing_leases.iter().all(|lease| {
                lease
                    .as_ref()
                    .is_some_and(|lease| lease.process_id == candidate.pid)
            });
            if existing_lease {
                // A lease already owns this transaction. The next refresh is
                // responsible for observing the endpoint move; never rewrite
                // it repeatedly while Core Audio is catching up.
                continue;
            }

            // A user override is an explicit revocation of qpwgraph's
            // ownership. Preserve it across a selector-matched process
            // restart instead of reapplying the private policy.
            if existing_leases.iter().flatten().any(|lease| !lease.owned) {
                continue;
            }

            let mut originals = BTreeMap::new();
            let mut read_failed = false;
            for (index, role) in Self::AUTOMATIC_APP_ROUTE_ROLES.iter().enumerate() {
                if let Some(lease) = existing_leases[index].as_ref() {
                    // Preserve the original endpoint captured for the first
                    // PID. Reading the replacement PID here would usually
                    // return `None` and would make a restart lose the safe
                    // restore target.
                    originals.insert(
                        *role,
                        lease
                            .original_endpoint
                            .as_ref()
                            .and_then(|endpoint| endpoint.current_mmdevice_id.clone()),
                    );
                    continue;
                }
                let Ok(original_id) = self.app_route_policy.get_persisted_endpoint(
                    &identity,
                    AudioFlow::Render,
                    *role,
                ) else {
                    read_failed = true;
                    break;
                };
                originals.insert(*role, original_id);
            }
            if read_failed {
                continue;
            }

            let mut attempted_roles = Vec::new();
            let mut set_failed = false;
            let target = virtual_output
                .current_mmdevice_id
                .as_deref()
                .unwrap_or_default();
            if target.is_empty() {
                continue;
            }
            for role in Self::AUTOMATIC_APP_ROUTE_ROLES {
                attempted_roles.push(role);
                if self
                    .app_route_policy
                    .set_persisted_endpoint(&identity, AudioFlow::Render, role, Some(target))
                    .is_err()
                {
                    set_failed = true;
                    break;
                }
            }
            if set_failed {
                // Best-effort rollback is still ownership-safe: if the PID or
                // identity changed, the policy boundary rejects the call and
                // the route remains manual/degraded rather than touching a
                // reused process.
                let mut rollback_failed_roles = Vec::new();
                for role in attempted_roles {
                    let original = originals
                        .get(&role)
                        .and_then(|endpoint| endpoint.as_deref());
                    if self
                        .app_route_policy
                        .rollback_persisted_endpoint(&identity, AudioFlow::Render, role, original)
                        .is_err()
                    {
                        rollback_failed_roles.push(role);
                    }
                }
                if !rollback_failed_roles.is_empty() {
                    // Keep ownership for any role that could not be restored.
                    // A later refresh after the policy is recreated can retry
                    // the restore, while a route that was already restored is
                    // not represented as an active lease.
                    self.application_route_policy_generation =
                        self.application_route_policy_generation.wrapping_add(1);
                    let generation = self.application_route_policy_generation;
                    for role in rollback_failed_roles {
                        let original_endpoint = self.selector_for_policy_endpoint(
                            originals
                                .get(&role)
                                .and_then(|endpoint| endpoint.as_deref()),
                        );
                        self.application_route_leases.insert(
                            (rule_index, role),
                            AutomaticAppRouteLease::new(
                                candidate.selector.clone(),
                                candidate.pid,
                                original_endpoint,
                                virtual_output.clone(),
                                generation,
                            ),
                        );
                    }
                }
                continue;
            }

            // A successful private setter is not by itself proof that the
            // per-role policy was committed. Read each role back before
            // publishing leases or allowing the ordinary route reconciler to
            // treat isolation as complete. A mismatch uses the same
            // ownership-safe rollback as a setter failure.
            if Self::AUTOMATIC_APP_ROUTE_ROLES.iter().any(|role| {
                !self
                    .app_route_policy
                    .get_persisted_endpoint(&identity, AudioFlow::Render, *role)
                    .is_ok_and(|current| {
                        Self::endpoint_id_matches(&virtual_output, current.as_deref())
                    })
            }) {
                set_failed = true;
            }
            if set_failed {
                let mut rollback_failed_roles = Vec::new();
                for role in Self::AUTOMATIC_APP_ROUTE_ROLES {
                    let original = originals
                        .get(&role)
                        .and_then(|endpoint| endpoint.as_deref());
                    if self
                        .app_route_policy
                        .rollback_persisted_endpoint(&identity, AudioFlow::Render, role, original)
                        .is_err()
                    {
                        rollback_failed_roles.push(role);
                    }
                }
                if !rollback_failed_roles.is_empty() {
                    self.application_route_policy_generation =
                        self.application_route_policy_generation.wrapping_add(1);
                    let generation = self.application_route_policy_generation;
                    for role in rollback_failed_roles {
                        let original_endpoint = self.selector_for_policy_endpoint(
                            originals
                                .get(&role)
                                .and_then(|endpoint| endpoint.as_deref()),
                        );
                        self.application_route_leases.insert(
                            (rule_index, role),
                            AutomaticAppRouteLease::new(
                                candidate.selector.clone(),
                                candidate.pid,
                                original_endpoint,
                                virtual_output.clone(),
                                generation,
                            ),
                        );
                    }
                }
                continue;
            }

            self.application_route_policy_generation =
                self.application_route_policy_generation.wrapping_add(1);
            let generation = self.application_route_policy_generation;
            for role in Self::AUTOMATIC_APP_ROUTE_ROLES {
                let original_endpoint = self.selector_for_policy_endpoint(
                    originals
                        .get(&role)
                        .and_then(|endpoint| endpoint.as_deref()),
                );
                self.application_route_leases.insert(
                    (rule_index, role),
                    AutomaticAppRouteLease::new(
                        candidate.selector.clone(),
                        candidate.pid,
                        original_endpoint,
                        virtual_output.clone(),
                        generation,
                    ),
                );
            }
            changed = true;
        }
        changed
    }

    pub(super) fn reconcile_application_route_snapshot(&mut self) {
        let mut captures = BTreeMap::new();
        for capture in &self.process_captures {
            let readiness = match &capture.state {
                ProcessCaptureState::Active => ProcessCaptureReadiness::Ready,
                ProcessCaptureState::Unavailable { reason }
                | ProcessCaptureState::Lost { reason } => {
                    ProcessCaptureReadiness::Failed(reason.clone())
                }
            };
            captures.insert((capture.key.selector.clone(), capture.key.pid), readiness);
        }
        let environment = ApplicationRouteEnvironment {
            // This is deliberately a capability boundary, not a version
            // guess. Unsupported activation is reported by the capture
            // readiness state and never becomes an active route.
            os_supported: true,
            virtual_driver_ready: matches!(
                self.virtual_driver_health,
                VirtualAudioDriverHealth::Ready { .. }
            ),
            effects_available: true,
            applications: self.application_route_candidates.clone(),
            endpoints: self.endpoint_selectors.values().cloned().collect(),
            captures,
        };
        self.application_routes
            .migrate_destination_selectors(&environment.endpoints);
        self.application_routes.reconcile(&environment);
    }

    fn sync_application_route_captures(&mut self) -> BackendResult<()> {
        let requests = self
            .application_routes
            .capture_requests()
            .into_iter()
            .filter_map(|(_, selector, pid)| {
                Some(ProcessCaptureRequest {
                    selector: selector.runtime_key()?,
                    pid,
                    mode: ProcessLoopbackMode::IncludeProcessTree,
                })
            })
            .collect();
        let (sender, receiver) = mpsc::channel();
        self.command_tx
            .send(WorkerCommand::ReconcileProcessCaptures(requests, sender))
            .map_err(|_| BackendError::Native("Windows audio worker is unavailable".into()))?;
        self.process_captures = Self::response(receiver)?;
        self.reconcile_application_route_snapshot();
        Ok(())
    }

    /// Route candidates that still need a process-loopback activation before
    /// their saved local route can be considered. The caller can feed these
    /// requests to the worker-owned capture manager and invoke reconciliation
    /// again after the manager reports `Active`.
    pub fn application_route_capture_requests(
        &self,
    ) -> Vec<(usize, pw_graph_config::WindowsApplicationSelector, u32)> {
        self.application_routes.capture_requests()
    }

    /// Return the reconciler's current rules after any legacy endpoint
    /// selector migration. The caller may persist this copy; live PIDs and
    /// native endpoint objects are never part of it.
    pub fn application_route_rules(&self) -> Vec<pw_graph_config::WindowsApplicationRoute> {
        self.application_routes.rules().to_vec()
    }

    /// Export a bounded, text-only Windows audio report for diagnostics.
    ///
    /// The report deliberately contains graph identities, capabilities, and
    /// route counters only. It never includes PCM samples, endpoint property
    /// blobs, pairing credentials, or other opaque native data, so it is safe
    /// to copy into a bug report.
    pub fn windows_audio_report(&self) -> String {
        use std::fmt::Write as _;

        let mut report = String::from("qpwgraph Windows audio report\n");
        let _ = writeln!(report, "os=Windows build={}", windows_os_build());
        let _ = writeln!(
            report,
            "virtual_driver={:?}\nnodes={} ports={} observed_links={} managed_links={}",
            self.virtual_driver_health,
            self.graph.nodes.len(),
            self.graph.ports.len(),
            self.graph
                .links
                .values()
                .filter(|link| !self.is_link_mutable(link.id))
                .count(),
            self.routing
                .as_ref()
                .map_or(0, |routing| routing.links().count()),
        );
        let _ = writeln!(
            report,
            "process_loopback_captures={}",
            self.process_captures.len()
        );
        let _ = writeln!(
            report,
            "process_loopback_support=operational_activation_probe_per_session"
        );
        let policy = self.app_route_policy.diagnostics();
        let _ = writeln!(
            report,
            "automatic_app_route_policy enabled={} os_build={} interface_version={:?} last_operation={:?} last_hresult={:?} fallback_reason={:?}",
            policy.enabled,
            policy.os_build,
            policy.interface_version,
            policy.last_operation,
            policy.last_hresult,
            policy.fallback_reason,
        );
        for capture in &self.process_captures {
            let _ = writeln!(
                report,
                "process_capture selector={:?} pid={} generation={} mode={:?} consumers={} state={:?} error={:?}",
                capture.key.selector,
                capture.key.pid,
                capture.key.generation,
                capture.key.mode,
                capture.consumers.len(),
                capture.state,
                capture.last_error,
            );
        }
        for node in self.graph.nodes.values() {
            let capabilities = self.node_capabilities(node.id);
            let _ = writeln!(
                report,
                "node id={} type={:?} name={:?} meter_peak={} meter_rms={} routable={}",
                node.id.0,
                node.node_type,
                node.name,
                capabilities.meter_peak,
                capabilities.meter_rms,
                self.node_supports_routing(node.id),
            );
        }
        for (mmdevice_id, selector) in &self.endpoint_selectors {
            let _ = writeln!(
                report,
                "endpoint mmdevice_id_hash={:016x} stable_id={:?} friendly_name={:?} flow={:?}",
                stable_local_id(mmdevice_id),
                selector.stable_id,
                selector.friendly_name,
                selector.data_flow,
            );
        }
        for identity in &self.virtual_endpoint_identities {
            let _ = writeln!(
                report,
                "virtual_endpoint role={:?} mmdevice_id_hash={:016x} stable_id={:?} driver_version={:?}",
                identity.role,
                stable_local_id(&identity.mmdevice_id),
                identity.stable_endpoint_id,
                identity.driver_version,
            );
        }
        for plan in self.application_routes.plans() {
            let app_key = plan
                .application
                .as_ref()
                .and_then(|application| application.selector.runtime_key())
                .unwrap_or_else(|| "<none>".into());
            let pid = plan
                .application
                .as_ref()
                .map_or(0, |application| application.pid);
            let destination = plan
                .destination
                .as_ref()
                .map(|endpoint| endpoint.stable_id.as_deref().unwrap_or("<no-stable-id>"))
                .unwrap_or("<none>");
            let _ = writeln!(
                report,
                "application_route rule={} state={:?} selector={:?} pid={} destination_stable_id={:?} reason={:?}",
                plan.rule_index, plan.state, app_key, pid, destination, plan.reason,
            );
        }
        #[cfg(feature = "relay")]
        if let Some(relay) = &self.relay {
            let config = relay.handle().config();
            let resolved_hash = relay.resolved_endpoint().map(stable_local_id);
            let _ = writeln!(
                report,
                "relay mode={:?} endpoint_active={} source={:?} sink={:?} resolved_id_hash={:?}",
                config.mode,
                relay.endpoint_active(),
                self.relay_send_source,
                self.relay_receive_sink,
                resolved_hash,
            );
        }
        #[cfg(feature = "relay")]
        for (selector, name, pid) in &self.relay_application_sources {
            let _ = writeln!(
                report,
                "relay_application selector={selector:?} name={name:?} pid={pid} active=capture-only",
            );
        }
        for (link, metrics) in self.route_metrics() {
            let Some(record) = self.graph.links.get(&link) else {
                continue;
            };
            let source = self
                .graph
                .ports
                .get(&record.output_port)
                .and_then(|port| self.graph.nodes.get(&port.node_id))
                .map(|node| node.name.as_str())
                .unwrap_or("<missing source>");
            let destination = self
                .graph
                .ports
                .get(&record.input_port)
                .and_then(|port| self.graph.nodes.get(&port.node_id))
                .map(|node| node.name.as_str())
                .unwrap_or("<missing destination>");
            let _ = writeln!(
                report,
                "route id={} source={:?} destination={:?} frames={} source_underruns={} sink_overruns={} discontinuities={} restarts={} fault={:?}",
                link.0,
                source,
                destination,
                metrics.frames_processed,
                metrics.source_underruns,
                metrics.sink_overruns,
                metrics.discontinuities,
                metrics.restarts,
                metrics.fault,
            );
        }
        report
    }

    pub(super) fn response<T>(receiver: Receiver<BackendResult<T>>) -> BackendResult<T> {
        receiver
            .recv()
            .map_err(|_| BackendError::Native("Windows audio worker stopped responding".into()))?
    }

    pub(super) fn refresh_snapshot(&mut self, only_if_needed: bool) -> BackendResult<()> {
        let (sender, receiver) = mpsc::channel();
        self.command_tx
            .send(if only_if_needed {
                WorkerCommand::RefreshIfNeeded(sender)
            } else {
                WorkerCommand::Refresh(sender)
            })
            .map_err(|_| BackendError::Native("Windows audio worker is unavailable".into()))?;
        let snapshot = Self::response(receiver)?;
        let mut graph = snapshot.graph;
        for (node_id, position) in &self.positions {
            if let Some(node) = graph.nodes.get_mut(node_id) {
                node.position = *position;
            }
        }
        self.endpoint_ports = snapshot.endpoint_ports;
        self.endpoint_selectors = snapshot.endpoint_selectors;
        self.process_audio_capabilities = snapshot.process_audio_capabilities;
        self.application_route_candidates = snapshot.application_route_candidates;
        self.application_route_ports = snapshot.application_route_ports;
        self.process_captures = snapshot.process_captures;
        self.virtual_endpoint_identities = snapshot.virtual_endpoint_identities;
        self.virtual_driver_health = snapshot.virtual_driver_health;
        if !self.application_routes.rules().is_empty() {
            self.reconcile_application_route_snapshot();
            self.sync_application_route_captures()?;
        } else {
            // Rules may have been removed after a previous refresh. Release
            // route-owned process captures as well as the graph links.
            if self.process_captures.iter().any(|capture| {
                capture
                    .consumers
                    .contains(&ProcessCaptureConsumer::OwnedRoute)
            }) {
                self.sync_application_route_captures()?;
            }
            self.clear_application_route_links()?;
        }
        // Core Audio has never heard of an effect, so the rebuilt graph has
        // no effect nodes in it. Draw them again before the links, or the
        // links that pass through them would have nowhere to land.
        for instance in self.effects.all_instances() {
            let name = self
                .effects
                .descriptors()
                .into_iter()
                .find(|descriptor| descriptor.id == instance.config.effect_id)
                .map(|descriptor| descriptor.name)
                .unwrap_or_else(|| instance.config.effect_id.clone());
            let position = self
                .effect_positions
                .get(&instance.config.instance_id)
                .copied()
                .unwrap_or_default();
            let _ = Self::draw_effect(&mut graph, instance, &name, position);
        }
        // The worker rebuilds the graph from what Core Audio reports, which
        // knows nothing about the routes qpwgraph is carrying. Drop the ones
        // whose devices have gone, then put the survivors back: a link the
        // user drew must not disappear because an unrelated endpoint changed.
        if let Some(routing) = self.routing.as_mut() {
            let live: BTreeSet<PortId> = graph.ports.keys().copied().collect();
            routing.reconcile(&live)?;
            routing.recover_lost(&self.endpoint_ports)?;
            for link in routing.links() {
                let _ = graph.insert_existing_link(link.clone());
            }
        }
        self.graph = graph;
        self.apply_application_route_plans()?;
        // Private policy changes happen after qpwgraph has removed any route
        // that is no longer requested. The current snapshot may still show
        // the old physical endpoint; Core Audio will notify the worker and a
        // later refresh must confirm AppRender isolation before activation.
        let _automatic_policy_changed = self.sync_automatic_application_route_policies();
        self.meterable = snapshot.meterable;
        // A refresh re-reads volumes from Core Audio, which knows only about
        // the part of the level it is holding. Multiply the route's software
        // gain back in, or a boosted node would appear to drop to unity every
        // time an unrelated device changed.
        self.restore_routed_gain();
        #[cfg(feature = "relay")]
        {
            let application_sources_changed =
                self.relay_application_sources != snapshot.application_sources;
            self.relay_endpoint_choices = snapshot.playback_endpoints;
            self.relay_input_choices = snapshot.capture_endpoints;
            self.relay_application_sources = snapshot.application_sources;
            self.relay_default_input = snapshot.default_input;
            self.relay_default_output = snapshot.default_output;
            let default_generation = snapshot.default_generation;
            let default_changed = self.relay_default_generation != default_generation;
            if default_changed {
                self.relay_default_generation = default_generation;
            }
            let endpoint_inactive = self
                .relay
                .as_ref()
                .is_some_and(|devices| !devices.endpoint_active());
            if default_changed || application_sources_changed || endpoint_inactive {
                // A process restart keeps the selector stable but changes its
                // live PID. Rebind the capture worker to the new process. If
                // the app has disappeared entirely, drop the worker rather
                // than silently attaching to an unrelated process.
                let selected_missing = matches!(
                    (self.relay_mode, &self.relay_send_source),
                    (
                        api::RelayMode::Emitter,
                        api::RelaySendSource::Application(selector)
                    ) if !self
                        .relay_application_sources
                        .iter()
                        .any(|(key, _, _)| key.eq_ignore_ascii_case(selector))
                );
                if selected_missing {
                    if let Some(devices) = self.relay.as_mut() {
                        devices.deactivate_application(
                            "Windows relay application source disappeared; process-loopback worker stopped",
                        );
                    }
                } else {
                    self.reconcile_relay_worker()?;
                }
            }
            self.sync_relay_capture_manager()?;
        }
        Ok(())
    }
}
