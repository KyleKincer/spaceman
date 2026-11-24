use crate::{
    AppInstanceId, AppObservation, BindingState, Decision, Model, NativeObservation,
    PhysicalWindowId, Placement, Reconciliation, ReconciliationEvidence, Settings, Slot, SlotId,
    config::LayoutMode,
    layout_tree::LayoutTree,
    model::{FastHashMap, FastHashSet},
    reconcile::reconcile_model,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CycleTicket {
    pub cycle: u64,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaleCycle {
    pub ticket: CycleTicket,
    pub current_revision: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Engine {
    model: Model,
    layout: LayoutTree,
    // Only the requested layout above is persisted. The fit is shared by
    // placement, navigation and HUD previews until its inputs change.
    fit: Option<DisplayFit>,
    insertion_focus: Option<crate::SlotId>,
    evidence: ReconciliationEvidence,
    revision: u64,
    persisted_revision: u64,
    next_cycle: u64,
    pending: Option<CycleTicket>,
    // O(1) lookup indices kept in sync with `model.workspace.slots`.
    slot_positions: FastHashMap<SlotId, usize>,
    identity_windows: FastHashMap<PhysicalWindowId, usize>,
    app_slot_counts: FastHashMap<AppInstanceId, usize>,
    unresolved_count: usize,
    cached_placements: Option<(Settings, Vec<Placement>)>,
}

#[derive(Clone, Debug)]
struct DisplayFit {
    output: crate::Rect,
    minimums: std::collections::HashMap<crate::SlotId, (i32, i32)>,
    tree: Option<LayoutTree>,
}

impl Engine {
    pub fn invalidate(&mut self) {
        self.pending = None;
        self.revision = self.revision.saturating_add(1);
    }

    pub fn begin_cycle(&mut self) -> CycleTicket {
        self.next_cycle = self.next_cycle.saturating_add(1);
        let ticket = CycleTicket {
            cycle: self.next_cycle,
            revision: self.revision,
        };
        self.pending = Some(ticket);
        ticket
    }

    pub fn accept(
        &mut self,
        ticket: CycleTicket,
        observation: &NativeObservation,
        settings: &Settings,
    ) -> Result<Reconciliation, StaleCycle> {
        let decisions = self.accept_in_place(ticket, observation, settings)?;
        let placements = self.placements_ref(settings).to_vec();
        Ok(Reconciliation {
            model: self.model.clone(),
            placements,
            decisions,
        })
    }

    /// Reconcile `observation` into `self` in place. On steady-state cycles
    /// where every Complete app's window set matches its bound slots, this
    /// avoids cloning `Model`, rebuilding layout trees, or allocating placements.
    pub(crate) fn accept_in_place(
        &mut self,
        ticket: CycleTicket,
        observation: &NativeObservation,
        settings: &Settings,
    ) -> Result<Vec<Decision>, StaleCycle> {
        if self.pending != Some(ticket) || observation.cycle != ticket.cycle {
            return Err(StaleCycle {
                ticket,
                current_revision: self.revision,
            });
        }
        self.pending = None;

        if self.try_steady_state_reconcile(observation) {
            if self
                .cached_placements
                .as_ref()
                .is_none_or(|(cached_settings, _)| cached_settings != settings)
            {
                self.refresh_cached_placements(settings);
            }
            return Ok(Vec::new());
        }

        let (next_model, decisions) =
            reconcile_model(&self.model, observation, &self.evidence, settings);
        let slots_changed = self.model.workspace.slots != next_model.workspace.slots
            || self.model.workspace.axis != next_model.workspace.axis;
        self.model = next_model;
        if slots_changed {
            self.rebuild_slot_indices();
            if self.layout.sync_near(
                self.model.workspace.axis,
                &self.model.workspace.slots,
                self.insertion_focus,
                settings.layout,
                settings.canvas(),
            ) {
                self.model.workspace.axis = self.layout.root_axis();
                self.fit = None;
                self.insertion_focus = self
                    .insertion_focus
                    .filter(|&id| self.slot_positions.contains_key(&id));
            }
            self.persisted_revision = self.persisted_revision.saturating_add(1);
            self.cached_placements = None;
        }
        self.refresh_cached_placements(settings);
        Ok(decisions)
    }

    fn try_steady_state_reconcile(&mut self, observation: &NativeObservation) -> bool {
        if observation.mouse_down || self.unresolved_count > 0 {
            return false;
        }
        let slots = &self.model.workspace.slots;
        let mut seen = vec![false; slots.len()];
        let mut seen_apps: FastHashSet<&AppInstanceId> = FastHashSet::with_capacity_and_hasher(
            observation.applications.len(),
            Default::default(),
        );
        for app_observation in &observation.applications {
            let AppObservation::Complete { app, windows, .. } = app_observation else {
                return false;
            };
            if !seen_apps.insert(&app.id) {
                return false;
            }
            let expected = self.app_slot_counts.get(&app.id).copied().unwrap_or(0);
            if windows.len() != expected {
                return false;
            }
            for window in windows {
                if window.id.app != app.id {
                    return false;
                }
                let Some(&index) = self.identity_windows.get(&window.id) else {
                    return false;
                };
                if seen[index] || slots[index].bound_window() != Some(&window.id) {
                    return false;
                }
                seen[index] = true;
            }
        }
        // Maintain identical `model.continuity` updates in place.
        for app_observation in &observation.applications {
            if let AppObservation::Complete { app, focused, .. } = app_observation {
                if let Some(entry) = self.model.continuity.get_mut(&app.id) {
                    if entry.previous_focused != *focused {
                        entry.previous_focused.clone_from(focused);
                    }
                } else {
                    self.model.continuity.insert(
                        app.id.clone(),
                        crate::model::AppContinuity {
                            previous_focused: focused.clone(),
                        },
                    );
                }
            }
        }
        true
    }

    fn refresh_cached_placements(&mut self, settings: &Settings) {
        let placements = self.compute_placements(settings);
        self.evidence.previous_placements.clear();
        self.evidence.previous_placements.reserve(placements.len());
        for placement in &placements {
            self.evidence
                .previous_placements
                .insert(placement.slot, placement.frame);
        }
        self.cached_placements = Some((settings.clone(), placements));
    }

    fn compute_placements(&self, settings: &Settings) -> Vec<Placement> {
        self.compute_placements_for(self.display_layout(), settings)
    }

    fn compute_placements_for(&self, layout: &LayoutTree, settings: &Settings) -> Vec<Placement> {
        settings
            .spacing
            .place(settings.usable(), layout.placements(settings.canvas()))
            .into_iter()
            .filter_map(|(id, frame)| {
                let slot = self.slot_by_id(id)?;
                Some(Placement {
                    slot: id,
                    window: slot.bound_window().cloned(),
                    frame,
                })
            })
            .collect()
    }

    pub(crate) fn placements_ref(&mut self, settings: &Settings) -> &[Placement] {
        let hit = self
            .cached_placements
            .as_ref()
            .is_some_and(|(cached_settings, _)| cached_settings == settings);
        if !hit {
            let placements = self.compute_placements(settings);
            return &self
                .cached_placements
                .insert((settings.clone(), placements))
                .1;
        }
        match self.cached_placements.as_ref() {
            Some((_, cached)) => cached,
            None => &[],
        }
    }

    #[inline]
    pub(crate) fn slot_by_id(&self, id: SlotId) -> Option<&Slot> {
        let &index = self.slot_positions.get(&id)?;
        self.model.workspace.slots.get(index)
    }

    #[inline]
    pub(crate) fn owns_window(&self, window: &PhysicalWindowId) -> bool {
        self.identity_windows.contains_key(window)
    }

    #[inline]
    pub(crate) fn owns_bound_window(&self, window: &PhysicalWindowId) -> bool {
        self.identity_windows
            .get(window)
            .and_then(|&index| self.model.workspace.slots.get(index))
            .is_some_and(|slot| slot.bound_window() == Some(window))
    }

    #[inline]
    pub(crate) fn owns_app(&self, app: &AppInstanceId) -> bool {
        self.app_slot_counts.contains_key(app)
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.model.workspace.slots.is_empty()
    }

    #[inline]
    pub(crate) const fn persisted_revision(&self) -> u64 {
        self.persisted_revision
    }

    fn rebuild_slot_indices(&mut self) {
        self.slot_positions.clear();
        self.identity_windows.clear();
        self.app_slot_counts.clear();
        self.unresolved_count = 0;
        let len = self.model.workspace.slots.len();
        self.slot_positions.reserve(len);
        self.identity_windows.reserve(len);
        for (index, slot) in self.model.workspace.slots.iter().enumerate() {
            self.slot_positions.insert(slot.id, index);
            let identity = slot.identity_window();
            self.identity_windows.insert(identity.clone(), index);
            *self
                .app_slot_counts
                .entry(identity.app.clone())
                .or_insert(0) += 1;
            if matches!(slot.binding, BindingState::Unresolved { .. }) {
                self.unresolved_count += 1;
            }
        }
    }

    #[must_use]
    pub const fn model(&self) -> &Model {
        &self.model
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn bound_windows(&self) -> Vec<PhysicalWindowId> {
        let mut windows = Vec::with_capacity(self.model.workspace.slots.len());
        self.layout.for_each_slot(&mut |id| {
            if let Some(window) = self.slot_by_id(id).and_then(Slot::bound_window) {
                windows.push(window.clone());
            }
        });
        windows
    }

    #[must_use]
    pub fn placements(&self, settings: &Settings) -> Vec<Placement> {
        if let Some((cached_settings, cached)) = &self.cached_placements
            && cached_settings == settings
        {
            return cached.clone();
        }
        self.compute_placements(settings)
    }

    pub(crate) fn record_placement_target(&mut self, slot: crate::SlotId, frame: crate::Rect) {
        self.evidence.previous_placements.insert(slot, frame);
    }

    pub(crate) fn toggle_axis(&mut self) {
        self.layout.toggle_root_axis();
        self.commit_layout();
    }

    #[cfg(test)]
    pub(crate) fn take_slot(&mut self, window: &PhysicalWindowId) -> Option<Slot> {
        self.take_slot_with(window, LayoutMode::Columns, crate::Rect::default())
    }

    pub(crate) fn take_slot_with(
        &mut self,
        window: &PhysicalWindowId,
        mode: LayoutMode,
        output: crate::Rect,
    ) -> Option<Slot> {
        let index = *self.identity_windows.get(window)?;
        if self.model.workspace.slots[index].bound_window() != Some(window) {
            return None;
        }
        let slot = self.model.workspace.slots.remove(index);
        self.rebuild_slot_indices();
        self.layout.sync_near(
            self.model.workspace.axis,
            &self.model.workspace.slots,
            None,
            mode,
            output,
        );
        self.model.workspace.axis = self.layout.root_axis();
        self.evidence.previous_placements.remove(&slot.id);
        self.invalidate_layout();
        Some(slot)
    }

    /// Remove every slot, bound or unresolved, whose identity belongs to `app`.
    pub(crate) fn forget_app_with(
        &mut self,
        app: &crate::AppInstanceId,
        mode: LayoutMode,
        output: crate::Rect,
    ) -> bool {
        if !self.app_slot_counts.contains_key(app) {
            self.model.continuity.remove(app);
            return false;
        }
        let before = self.model.workspace.slots.len();
        self.model
            .workspace
            .slots
            .retain(|slot| !slot.belongs_to(app));
        self.model.continuity.remove(app);
        if self.model.workspace.slots.len() == before {
            return false;
        }
        self.rebuild_slot_indices();
        let positions = &self.slot_positions;
        self.evidence
            .previous_placements
            .retain(|id, _| positions.contains_key(id));
        self.layout.sync_near(
            self.model.workspace.axis,
            &self.model.workspace.slots,
            None,
            mode,
            output,
        );
        self.model.workspace.axis = self.layout.root_axis();
        self.invalidate_layout();
        true
    }

    pub(crate) fn insert_slot(&mut self, slot: Slot) {
        self.insert_slot_with(slot, LayoutMode::Columns, crate::Rect::default(), None);
    }

    pub(crate) fn insert_slot_with(
        &mut self,
        mut slot: Slot,
        mode: LayoutMode,
        output: crate::Rect,
        anchor: Option<crate::SlotId>,
    ) {
        if self.slot_positions.contains_key(&slot.id) {
            slot.id = crate::SlotId(self.model.next_slot_id);
        }
        self.model.next_slot_id = self.model.next_slot_id.max(slot.id.0.saturating_add(1));
        self.model.workspace.slots.push(slot);
        self.rebuild_slot_indices();
        self.layout.sync_near(
            self.model.workspace.axis,
            &self.model.workspace.slots,
            anchor,
            mode,
            output,
        );
        self.model.workspace.axis = self.layout.root_axis();
        self.invalidate_layout();
    }

    pub(crate) fn swap_windows(
        &mut self,
        first: &PhysicalWindowId,
        second: &PhysicalWindowId,
    ) -> bool {
        let Some(second) = self.slot_for_window(second) else {
            return false;
        };
        self.edit_requested(first, |layout, first| layout.swap(first, second))
    }

    /// Reads a window the user resized to `current` as the split boundaries it moved.
    pub(crate) fn resize_window(
        &mut self,
        window: &PhysicalWindowId,
        current: crate::Rect,
        settings: &Settings,
    ) -> bool {
        let canvas = settings.canvas();
        let current = settings.spacing.untile(current);
        self.edit_weights(window, |layout, slot| layout.resize(slot, current, canvas))
    }

    pub(crate) fn remember_insertion_focus(&mut self, window: &PhysicalWindowId) {
        self.insertion_focus = self.slot_for_window(window);
    }

    fn display_layout(&self) -> &LayoutTree {
        self.fit
            .as_ref()
            .and_then(|fit| fit.tree.as_ref())
            .unwrap_or(&self.layout)
    }

    /// Re-evaluate even without a new refusal: a removed sibling or a larger
    /// display can make the original split usable again.
    fn slot_minimums(
        &self,
        settings: &Settings,
        minimums: &std::collections::HashMap<PhysicalWindowId, (i32, i32)>,
    ) -> std::collections::HashMap<crate::SlotId, (i32, i32)> {
        if minimums.is_empty() {
            return std::collections::HashMap::new();
        }
        // Every cell gives one whole gap to the space around its window.
        let gap = settings.spacing.inner;
        self.model
            .workspace
            .slots
            .iter()
            .filter_map(|slot| {
                let (width, height) = *minimums.get(slot.bound_window()?)?;
                Some((slot.id, (width + gap, height + gap)))
            })
            .collect()
    }

    /// Compute placements for a candidate layout tree (including any temporary
    /// minimum-size fit) without mutating the committed workspace layout.
    pub(crate) fn placements_for_tree(
        &self,
        layout: &LayoutTree,
        settings: &Settings,
        minimums: &std::collections::HashMap<PhysicalWindowId, (i32, i32)>,
    ) -> Vec<Placement> {
        let slots = self.slot_minimums(settings, minimums);
        let fitted = (!slots.is_empty())
            .then(|| layout.fitted(settings.canvas(), &slots))
            .flatten();
        self.compute_placements_for(fitted.as_ref().unwrap_or(layout), settings)
    }

    /// Commit an already-edited layout tree as the workspace's requested layout.
    pub(crate) fn commit_layout_tree(&mut self, layout: LayoutTree) {
        self.layout = layout;
        self.commit_layout();
    }

    pub(crate) fn fit_minimums(
        &mut self,
        settings: &Settings,
        minimums: &std::collections::HashMap<PhysicalWindowId, (i32, i32)>,
    ) -> bool {
        let output = settings.canvas();
        let slots = self.slot_minimums(settings, minimums);
        if slots.is_empty() {
            let changed = self.fit.take().is_some_and(|fit| fit.tree.is_some());
            if changed {
                self.cached_placements = None;
            }
            return changed;
        }
        if self
            .fit
            .as_ref()
            .is_some_and(|fit| fit.output == output && fit.minimums == slots)
        {
            return false;
        }
        let tree = self.layout.fitted(output, &slots);
        let changed = self.fit.as_ref().and_then(|fit| fit.tree.as_ref()) != tree.as_ref();
        self.fit = Some(DisplayFit {
            output,
            minimums: slots,
            tree,
        });
        if changed {
            self.cached_placements = None;
        }
        changed
    }

    pub(crate) fn resize_focused_tile(
        &mut self,
        window: &PhysicalWindowId,
        direction: crate::ResizeDirection,
        amount: i32,
        settings: &Settings,
    ) -> bool {
        let canvas = settings.canvas();
        self.edit_weights(window, |layout, slot| {
            layout.resize_by(slot, direction, amount, canvas)
        })
    }

    pub(crate) fn balance_focused_parent(
        &mut self,
        window: &PhysicalWindowId,
        settings: &Settings,
    ) -> bool {
        let canvas = settings.canvas();
        self.edit_weights(window, |layout, slot| layout.balance(slot, canvas))
    }

    /// Resize the focused window so its frame is `points` along `axis`: the tile's cell is the
    /// window plus the inner gap.
    pub(crate) fn resize_focused_tile_to(
        &mut self,
        window: &PhysicalWindowId,
        axis: crate::Axis,
        points: i32,
        settings: &Settings,
    ) -> bool {
        let canvas = settings.canvas();
        let cell = settings.spacing.untile(crate::Rect {
            x: 0,
            y: 0,
            width: points,
            height: points,
        });
        let target = match axis {
            crate::Axis::Horizontal => cell.width,
            crate::Axis::Vertical => cell.height,
        };
        self.edit_weights(window, |layout, slot| {
            layout.resize_to(slot, axis, target, canvas)
        })
    }

    pub(crate) fn create_implicit_split(
        &mut self,
        window: &PhysicalWindowId,
        direction: crate::Direction,
    ) -> bool {
        self.edit_requested(window, |layout, slot| {
            layout.create_implicit_split(slot, direction)
        })
    }

    #[cfg(test)]
    pub(crate) fn move_window_relative(
        &mut self,
        moved: &PhysicalWindowId,
        target: &PhysicalWindowId,
        direction: crate::Direction,
    ) -> bool {
        let Some(target) = self.slot_for_window(target) else {
            return false;
        };
        self.edit_requested(moved, |layout, moved| {
            layout.move_relative(moved, target, direction)
        })
    }

    pub(crate) fn layout(&self) -> &LayoutTree {
        &self.layout
    }

    pub(crate) fn root_axis(&self) -> crate::Axis {
        self.display_layout().root_axis()
    }

    /// Each tile's slot, window and frame in `output`, in layout order.
    pub(crate) fn preview_placements(&self, output: crate::Rect) -> Vec<Placement> {
        self.compute_placements(&Settings::for_output(output))
    }

    pub(crate) fn preview(&self, output: crate::Rect) -> Vec<crate::Rect> {
        self.display_layout().preview_rects(output)
    }

    pub(crate) fn restore_layout(&mut self, mut layout: LayoutTree) {
        layout.sync(self.model.workspace.axis, &self.model.workspace.slots);
        self.layout = layout;
        self.commit_layout();
    }

    pub(crate) fn slot_for_window(&self, window: &PhysicalWindowId) -> Option<crate::SlotId> {
        let &index = self.identity_windows.get(window)?;
        let slot = self.model.workspace.slots.get(index)?;
        (slot.bound_window() == Some(window)).then_some(slot.id)
    }

    /// Edits the layout on display, a fit included, and saves it. Only for
    /// size edits through a fit that keeps the requested grouping (see
    /// `edit_weights`); structural edits go through `edit_requested`.
    fn edit_shown(
        &mut self,
        window: &PhysicalWindowId,
        op: impl FnOnce(&mut LayoutTree, crate::SlotId) -> bool,
    ) -> bool {
        let Some(slot) = self.slot_for_window(window) else {
            return false;
        };
        let mut layout = self.display_layout().clone();
        if !op(&mut layout, slot) {
            return false;
        }
        self.layout = layout;
        self.commit_layout();
        true
    }

    /// Edits the requested layout, never the temporary fit on display, so a
    /// structural edit made while a fit stacks tiles does not save the stack.
    fn edit_requested(
        &mut self,
        window: &PhysicalWindowId,
        op: impl FnOnce(&mut LayoutTree, crate::SlotId) -> bool,
    ) -> bool {
        let Some(slot) = self.slot_for_window(window) else {
            return false;
        };
        let mut layout = self.layout.clone();
        if !op(&mut layout, slot) {
            return false;
        }
        self.layout = layout;
        self.commit_layout();
        true
    }

    /// Edits tile sizes. While the display shows an automatic grouping the
    /// requested layout lacks, the edit changes only that temporary fit:
    /// saving it would make the grouping permanent.
    fn edit_weights(
        &mut self,
        window: &PhysicalWindowId,
        op: impl FnOnce(&mut LayoutTree, crate::SlotId) -> bool,
    ) -> bool {
        let requested = &self.layout;
        let Some(tree) = self
            .fit
            .as_mut()
            .and_then(|fit| fit.tree.as_mut())
            .filter(|tree| tree.regroups(requested))
        else {
            return self.edit_shown(window, op);
        };
        let Some(&index) = self.identity_windows.get(window) else {
            return false;
        };
        let slot_entry = &self.model.workspace.slots[index];
        if slot_entry.bound_window() != Some(window) {
            return false;
        }
        let slot = slot_entry.id;
        let mut edited = tree.clone();
        if !op(&mut edited, slot) {
            return false;
        }
        *tree = edited;
        self.cached_placements = None;
        self.invalidate();
        true
    }

    fn invalidate_layout(&mut self) {
        self.fit = None;
        self.cached_placements = None;
        self.persisted_revision = self.persisted_revision.saturating_add(1);
        self.insertion_focus = self
            .insertion_focus
            .filter(|&id| self.slot_positions.contains_key(&id));
        self.invalidate();
    }

    fn commit_layout(&mut self) {
        self.model.workspace.axis = self.layout.root_axis();
        for slot in &mut self.model.workspace.slots {
            if let Some(weight) = self.layout.weight(slot.id) {
                slot.weight = weight;
            }
        }
        self.invalidate_layout();
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        AppInstanceId, AppObservation, NativeObservation, ObservedApplication, ObservedWindow,
        PhysicalWindowId, Rect,
    };

    use super::*;

    fn slot(id: u64, weight: u32) -> Slot {
        Slot {
            id: crate::SlotId(id),
            weight,
            binding: crate::BindingState::Bound(PhysicalWindowId {
                app: AppInstanceId {
                    pid: 1,
                    launched_at: 1,
                },
                window_id: id as u32,
            }),
        }
    }

    #[test]
    fn new_window_joins_the_focused_nested_split() {
        let mut engine = Engine::default();
        for i in 1..=3 {
            engine.insert_slot(slot(i, 1));
        }
        let windows = engine.bound_windows();
        assert!(engine.move_window_relative(&windows[0], &windows[1], crate::Direction::Up));
        engine.remember_insertion_focus(&windows[0]);
        let ticket = engine.begin_cycle();
        let columns = settings().with_layout(LayoutMode::Columns);
        engine
            .accept(ticket, &observation(ticket.cycle, 4), &columns)
            .unwrap();
        let output = crate::Rect {
            width: 1200,
            height: 800,
            ..crate::Rect::default()
        };
        let tiles = engine.layout.placements(output);
        assert_eq!(
            tiles.iter().map(|(s, _)| s.0).collect::<Vec<_>>(),
            [1, 4, 2, 3]
        );
        assert_eq!(tiles[0].1.width, 600);
        assert_eq!(tiles[1].1.width, 600);
        assert_eq!(
            tiles[3].1,
            crate::Rect {
                x: 600,
                width: 600,
                ..output
            }
        );
    }

    #[test]
    fn dwindle_splits_the_focused_tile_along_its_longer_side() {
        let mut engine = Engine::default();
        for i in 1..=3 {
            engine.insert_slot(slot(i, 1));
        }
        let windows = engine.bound_windows();
        assert!(engine.move_window_relative(&windows[0], &windows[1], crate::Direction::Up));
        engine.remember_insertion_focus(&windows[0]);
        let output = crate::Rect {
            width: 1200,
            height: 800,
            ..crate::Rect::default()
        };
        let ticket = engine.begin_cycle();
        engine
            .accept(
                ticket,
                &observation(ticket.cycle, 4),
                &Settings::for_output(output),
            )
            .unwrap();
        // Tile 1 was 600x400 (wider than tall), so window 4 splits it horizontally.
        let tiles = engine.layout.placements(output);
        assert_eq!(
            tiles.iter().map(|(s, _)| s.0).collect::<Vec<_>>(),
            [1, 4, 2, 3]
        );
        assert_eq!(
            tiles[0].1,
            crate::Rect {
                x: 0,
                y: 0,
                width: 300,
                height: 400,
            }
        );
        assert_eq!(
            tiles[1].1,
            crate::Rect {
                x: 300,
                y: 0,
                width: 300,
                height: 400,
            }
        );
    }

    #[test]
    fn structural_layout_edits_commit_axis_and_slot_weights() {
        let mut engine = Engine::default();
        let slots = [slot(1, 1), slot(2, 1), slot(3, 1)];
        let windows = slots
            .iter()
            .map(|slot| slot.bound_window().unwrap().clone())
            .collect::<Vec<_>>();
        for slot in slots {
            engine.insert_slot(slot);
        }
        let output = Rect {
            width: 1000,
            height: 800,
            ..Rect::default()
        };
        assert!(engine.resize_focused_tile(
            &windows[0],
            crate::ResizeDirection::Grow,
            50,
            &Settings::for_output(output)
        ));
        assert!(engine.create_implicit_split(&windows[1], crate::Direction::Up));
        assert_eq!(engine.model().workspace.axis, crate::Axis::Vertical);
        assert_eq!(engine.model().workspace.slots[1].weight, 1);

        assert!(engine.move_window_relative(&windows[2], &windows[0], crate::Direction::Right));
        assert_eq!(engine.model().workspace.slots[2].weight, 1);
    }

    fn observation(cycle: u64, window_id: u32) -> NativeObservation {
        let app = AppInstanceId {
            pid: 10,
            launched_at: 1,
        };
        let window = PhysicalWindowId {
            app: app.clone(),
            window_id,
        };
        NativeObservation {
            cycle,
            mouse_down: false,
            applications: vec![AppObservation::Complete {
                app: ObservedApplication {
                    id: app,
                    bundle_id: "com.example.Editor".into(),
                },
                focused: Some(window.clone()),
                windows: vec![ObservedWindow {
                    id: window,
                    frame: Rect {
                        width: 100,
                        height: 100,
                        ..Rect::default()
                    },
                    resizable: true,
                    title: None,
                }],
            }],
        }
    }

    fn settings() -> Settings {
        Settings {
            output: Rect {
                width: 100,
                height: 100,
                ..Rect::default()
            },
            frame_tolerance: 2,
            spacing: crate::spacing::Spacing::NONE,
            layout: LayoutMode::default(),
        }
    }

    #[test]
    fn accepted_cycle_commits_model_and_evidence() {
        let mut engine = Engine::default();
        engine.invalidate();
        let ticket = engine.begin_cycle();
        let result = engine.accept(ticket, &observation(ticket.cycle, 1), &settings());

        assert_eq!(result.unwrap().model.workspace.slots.len(), 1);
        assert_eq!(engine.model().workspace.slots.len(), 1);
    }

    #[test]
    fn invalidation_rejects_an_in_flight_cycle_without_mutation() {
        let mut engine = Engine::default();
        engine.invalidate();
        let ticket = engine.begin_cycle();
        engine.invalidate();

        let error = engine
            .accept(ticket, &observation(ticket.cycle, 1), &settings())
            .unwrap_err();
        assert_eq!(error.current_revision, 2);
        assert!(engine.model().workspace.slots.is_empty());
    }

    #[test]
    fn mismatched_observation_cycle_is_rejected() {
        let mut engine = Engine::default();
        let ticket = engine.begin_cycle();

        assert!(
            engine
                .accept(ticket, &observation(ticket.cycle + 1, 1), &settings())
                .is_err()
        );
    }

    #[test]
    fn newer_cycle_supersedes_an_older_ticket_in_the_same_revision() {
        let mut engine = Engine::default();
        let older = engine.begin_cycle();
        let current = engine.begin_cycle();

        assert!(
            engine
                .accept(older, &observation(older.cycle, 1), &settings())
                .is_err()
        );
        assert!(
            engine
                .accept(current, &observation(current.cycle, 1), &settings())
                .is_ok()
        );
    }

    #[test]
    fn accepted_ticket_cannot_be_committed_twice() {
        let mut engine = Engine::default();
        let ticket = engine.begin_cycle();
        let observation = observation(ticket.cycle, 1);

        assert!(engine.accept(ticket, &observation, &settings()).is_ok());
        assert!(engine.accept(ticket, &observation, &settings()).is_err());
    }

    #[test]
    fn reordering_a_window_invalidates_pending_work() {
        let first = PhysicalWindowId {
            app: AppInstanceId {
                pid: 1,
                launched_at: 1,
            },
            window_id: 1,
        };
        let second = PhysicalWindowId {
            window_id: 2,
            ..first.clone()
        };
        let mut engine = Engine::default();
        engine.insert_slot(Slot {
            id: crate::SlotId(1),
            weight: 1,
            binding: crate::BindingState::Bound(first.clone()),
        });
        engine.insert_slot(Slot {
            id: crate::SlotId(2),
            weight: 1,
            binding: crate::BindingState::Bound(second.clone()),
        });
        let pending = engine.begin_cycle();

        assert!(engine.swap_windows(&second, &first));
        assert_eq!(engine.bound_windows(), vec![second, first]);
        assert!(
            engine
                .accept(pending, &observation(pending.cycle, 1), &settings())
                .is_err()
        );
    }

    #[test]
    fn taking_a_slot_invalidates_pending_work() {
        let window = PhysicalWindowId {
            app: AppInstanceId {
                pid: 1,
                launched_at: 1,
            },
            window_id: 1,
        };
        let mut engine = Engine::default();
        engine.insert_slot(Slot {
            id: crate::SlotId(1),
            weight: 1,
            binding: crate::BindingState::Bound(window.clone()),
        });
        let pending = engine.begin_cycle();

        assert!(engine.take_slot(&window).is_some());
        assert!(
            engine
                .accept(pending, &observation(pending.cycle, 1), &settings())
                .is_err()
        );
        assert!(engine.model().workspace.slots.is_empty());
    }

    #[test]
    fn inserting_a_slot_invalidates_pending_work() {
        let mut engine = Engine::default();
        let pending = engine.begin_cycle();
        engine.insert_slot(Slot {
            id: crate::SlotId(1),
            weight: 1,
            binding: crate::BindingState::Bound(PhysicalWindowId {
                app: AppInstanceId {
                    pid: 1,
                    launched_at: 1,
                },
                window_id: 1,
            }),
        });

        assert!(
            engine
                .accept(pending, &observation(pending.cycle, 1), &settings())
                .is_err()
        );
        assert_eq!(engine.model().workspace.slots.len(), 1);
    }

    #[test]
    fn steady_state_accept_in_place_updates_continuity_and_preserves_revision() {
        let mut engine = Engine::default();
        let app = AppInstanceId {
            pid: 7,
            launched_at: 42,
        };
        let w1 = PhysicalWindowId {
            app: app.clone(),
            window_id: 1,
        };
        let w2 = PhysicalWindowId {
            app: app.clone(),
            window_id: 2,
        };
        let make_obs = |cycle: u64, focused: Option<PhysicalWindowId>| NativeObservation {
            cycle,
            mouse_down: false,
            applications: vec![AppObservation::Complete {
                app: crate::ObservedApplication {
                    id: app.clone(),
                    bundle_id: "com.example.App".into(),
                },
                focused,
                windows: vec![
                    crate::ObservedWindow {
                        id: w1.clone(),
                        frame: crate::Rect {
                            x: 0,
                            y: 0,
                            width: 500,
                            height: 800,
                        },
                        resizable: true,
                        title: None,
                    },
                    crate::ObservedWindow {
                        id: w2.clone(),
                        frame: crate::Rect {
                            x: 500,
                            y: 0,
                            width: 500,
                            height: 800,
                        },
                        resizable: true,
                        title: None,
                    },
                ],
            }],
        };

        let s = settings();
        let t1 = engine.begin_cycle();
        let first = engine
            .accept(t1, &make_obs(t1.cycle, Some(w1.clone())), &s)
            .unwrap();
        let rev_after_first = engine.persisted_revision();

        // Steady-state cycle with changed focus must update continuity without bumping persisted revision.
        let t2 = engine.begin_cycle();
        let decisions = engine
            .accept_in_place(t2, &make_obs(t2.cycle, Some(w2.clone())), &s)
            .unwrap();
        assert!(decisions.is_empty());
        assert_eq!(engine.persisted_revision(), rev_after_first);
        assert_eq!(
            engine
                .model()
                .continuity
                .get(&app)
                .and_then(|c| c.previous_focused.as_ref()),
            Some(&w2)
        );
        assert_eq!(engine.placements_ref(&s), first.placements.as_slice());
    }
}
