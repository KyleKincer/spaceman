use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::Axis;

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct AppInstanceId {
    pub pid: i32,
    pub launched_at: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PhysicalWindowId {
    pub app: AppInstanceId,
    pub window_id: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SlotId(pub u64);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BindingState {
    Bound(PhysicalWindowId),
    Unresolved {
        previous: PhysicalWindowId,
        candidates: Vec<PhysicalWindowId>,
        since: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Slot {
    pub id: SlotId,
    pub weight: u32,
    pub binding: BindingState,
}

impl Slot {
    #[must_use]
    pub fn bound_window(&self) -> Option<&PhysicalWindowId> {
        match &self.binding {
            BindingState::Bound(window) => Some(window),
            BindingState::Unresolved { .. } => None,
        }
    }

    #[must_use]
    pub fn belongs_to(&self, app: &AppInstanceId) -> bool {
        &self.identity_window().app == app
    }

    #[must_use]
    pub(crate) fn identity_window(&self) -> &PhysicalWindowId {
        match &self.binding {
            BindingState::Bound(window) => window,
            BindingState::Unresolved { previous, .. } => previous,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AppContinuity {
    pub previous_focused: Option<PhysicalWindowId>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Workspace {
    pub axis: Axis,
    pub slots: Vec<Slot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Model {
    pub workspace: Workspace,
    #[serde(with = "crate::serde_map")]
    pub continuity: HashMap<AppInstanceId, AppContinuity>,
    pub next_slot_id: u64,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            workspace: Workspace::default(),
            continuity: HashMap::new(),
            next_slot_id: 1,
        }
    }
}

impl Model {
    pub fn validate(&self) -> Result<(), String> {
        let mut slot_ids = std::collections::HashSet::new();
        let mut windows = std::collections::HashSet::new();
        for slot in &self.workspace.slots {
            if slot.weight == 0 {
                return Err(format!("slot {} has zero weight", slot.id.0));
            }
            if !slot_ids.insert(slot.id) {
                return Err(format!("duplicate slot ID {}", slot.id.0));
            }
            let window = slot.identity_window();
            if !windows.insert(window) {
                return Err(format!(
                    "window {} is owned more than once",
                    window.window_id
                ));
            }
            match &slot.binding {
                BindingState::Bound(_) => {}
                BindingState::Unresolved {
                    previous,
                    candidates,
                    ..
                } => {
                    if candidates
                        .iter()
                        .any(|candidate| candidate.app != previous.app)
                    {
                        return Err(format!(
                            "slot {} has cross-application candidates",
                            slot.id.0
                        ));
                    }
                }
            }
        }
        if self
            .workspace
            .slots
            .iter()
            .any(|slot| slot.id.0 >= self.next_slot_id)
        {
            return Err("next_slot_id does not exceed every existing slot ID".into());
        }
        Ok(())
    }
}

/// Fast non-cryptographic hasher (FxHash) for internal window/slot ID maps
/// whose keys come from trusted local macOS PIDs and CGWindowIDs.
#[derive(Default)]
pub(crate) struct FastHasher(u64);

const FX_K: u64 = 0x517c_c1b7_2722_0a95;

impl FastHasher {
    #[inline]
    fn mix(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(FX_K);
    }
}

impl std::hash::Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while let Some((chunk, rest)) = bytes.split_first_chunk::<8>() {
            self.mix(u64::from_ne_bytes(*chunk));
            bytes = rest;
        }
        if let Some((chunk, rest)) = bytes.split_first_chunk::<4>() {
            self.mix(u64::from(u32::from_ne_bytes(*chunk)));
            bytes = rest;
        }
        for &byte in bytes {
            self.mix(u64::from(byte));
        }
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.mix(u64::from(i));
    }

    #[inline]
    fn write_i32(&mut self, i: i32) {
        self.mix(i as u32 as u64);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.mix(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.mix(i as u64);
    }
}

pub(crate) type FastBuildHasher = std::hash::BuildHasherDefault<FastHasher>;
pub(crate) type FastHashMap<K, V> = HashMap<K, V, FastBuildHasher>;
pub(crate) type FastHashSet<K> = std::collections::HashSet<K, FastBuildHasher>;
