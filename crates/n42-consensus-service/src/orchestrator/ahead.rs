//! What a leader that builds ahead of consensus keeps, per view.
//!
//! With `N42_BUILD_AHEAD_DEPTH=2` a leader that leads the next views may
//! have two own blocks past the current view's under way: the next view's,
//! and the one after it, built on the next view's sealed output. Every
//! record here is therefore keyed by its build context (view and parent),
//! never a single slot that a later view's record would overwrite. Nothing
//! here decides a proposal: a payload built ahead is still released only in
//! its own view, and only when the LockedQC then names its parent.

use super::{OutputBuild, PayloadBuildContext, PayloadBuildReady};
use alloy_primitives::B256;

/// Own blocks kept as execution-layer tips when building two blocks ahead.
const TIPS: usize = 4;

/// The leader's records of blocks built ahead of their views.
#[derive(Debug, Default)]
pub(super) struct AheadState {
    /// Payloads built for views that have not started, one per view.
    pub(super) ready: Vec<PayloadBuildReady>,
    /// Sealed reports of own blocks too far ahead to build a child on yet.
    pub(super) sealed: Vec<PayloadBuildReady>,
    /// Validated reports of own blocks too far ahead to build a child on yet.
    pub(super) validated: Vec<PayloadBuildReady>,
    /// Child builds running on an own block's output, until they complete.
    pub(super) output_builds: Vec<OutputBuild>,
    /// `(tip, parent)` of own blocks the execution layer accepted ahead of
    /// their commit, oldest first. A finalize of `parent` keeps the newest
    /// tip on that chain as head.
    pub(super) tips: Vec<(B256, B256)>,
    /// Builds in flight for views that have not started. Kept apart from
    /// `building_on_parent`, which guards the current view's build until its
    /// proposal is released.
    pub(super) building: Vec<PayloadBuildContext>,
    /// Own blocks built ahead whose proposal is still being prepared. The
    /// block's child build has taken its place in `building` by then, so
    /// this is what keeps the block's own view from starting a second build
    /// on the same parent.
    pub(super) emitting: Vec<PayloadBuildContext>,
}

/// What [`AheadState::take_release`] leaves for a view that starts.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Release {
    /// The payload built ahead for the view, whose parent the LockedQC names.
    pub(super) released: Option<PayloadBuildReady>,
    /// Payloads built ahead that no longer fit: of a past view, or of this
    /// view on a parent the LockedQC does not name.
    pub(super) discarded: Vec<PayloadBuildReady>,
}

impl AheadState {
    /// Forgets every view's record except the execution-layer tips, which
    /// stay valid for the finalize of their parents.
    pub(super) fn clear(&mut self) {
        self.ready.clear();
        self.sealed.clear();
        self.validated.clear();
        self.output_builds.clear();
        self.building.clear();
        self.emitting.clear();
    }

    /// A build in flight on `parent`.
    pub(super) fn building_on(&self, parent: B256) -> Option<PayloadBuildContext> {
        self.building
            .iter()
            .copied()
            .find(|building| building.parent_hash == parent)
    }

    pub(super) fn start_building(&mut self, context: PayloadBuildContext) {
        if !self.building.contains(&context) {
            self.building.push(context);
        }
    }

    pub(super) fn end_building(&mut self, context: PayloadBuildContext) {
        self.building.retain(|building| *building != context);
    }

    pub(super) fn is_emitting(&self, context: PayloadBuildContext) -> bool {
        self.emitting.contains(&context)
    }

    pub(super) fn start_emitting(&mut self, context: PayloadBuildContext) {
        if !self.emitting.contains(&context) {
            self.emitting.push(context);
        }
    }

    pub(super) fn end_emitting(&mut self, context: PayloadBuildContext) {
        self.emitting.retain(|emitting| *emitting != context);
    }

    /// Records the own block `tip` on `parent` as accepted, keeping the
    /// newest `capacity` tips.
    pub(super) fn record_tip(&mut self, tip: B256, parent: B256, capacity: usize) {
        self.tips.retain(|(held, _)| *held != tip);
        self.tips.push((tip, parent));
        while self.tips.len() > capacity.max(1) {
            self.tips.remove(0);
        }
    }

    /// The head to announce when `committed` is finalized: the newest own
    /// tip that descends from `committed` through recorded tips, or
    /// `committed` itself. Never an ancestor of a recorded tip on that chain.
    pub(super) fn head_for(&self, committed: B256) -> B256 {
        let mut head = committed;
        for _ in 0..self.tips.len() {
            match self.tips.iter().rev().find(|(_, parent)| *parent == head) {
                Some((tip, _)) => head = *tip,
                None => break,
            }
        }
        head
    }

    /// Holds a payload built for a later view; a later build for the same
    /// view replaces it.
    pub(super) fn hold_payload(&mut self, ready: PayloadBuildReady) {
        self.ready
            .retain(|held| held.context.view != ready.context.view);
        self.ready.push(ready);
    }

    pub(super) fn output_build(&self, context: PayloadBuildContext) -> Option<&OutputBuild> {
        self.output_builds
            .iter()
            .find(|build| build.context == context)
    }

    pub(super) fn output_build_mut(
        &mut self,
        context: PayloadBuildContext,
    ) -> Option<&mut OutputBuild> {
        self.output_builds
            .iter_mut()
            .find(|build| build.context == context)
    }

    pub(super) fn start_output_build(&mut self, context: PayloadBuildContext) {
        self.output_builds.retain(|build| build.context != context);
        self.output_builds.push(OutputBuild {
            context,
            produced: false,
            validated: None,
        });
    }

    /// Ends the record of the output build of `context`.
    pub(super) fn take_output_build(&mut self, context: PayloadBuildContext) -> Option<OutputBuild> {
        let at = self
            .output_builds
            .iter()
            .position(|build| build.context == context)?;
        Some(self.output_builds.remove(at))
    }

    /// Whether a build of `child` exists already in any form: running on
    /// its parent's output or through a forkchoice update, produced and held
    /// for its view, or produced and on its way to its proposal.
    pub(super) fn child_covered(&self, child: PayloadBuildContext) -> bool {
        self.output_build(child).is_some()
            || self.building.contains(&child)
            || self.emitting.contains(&child)
            || self.ready.iter().any(|held| held.context == child)
    }

    /// On entering `new_view`: the payload held for it if `required` (the
    /// context the LockedQC allows) is the one it was built under. Every
    /// payload held for `new_view` or an earlier view leaves the list.
    pub(super) fn take_release(
        &mut self,
        new_view: u64,
        required: Option<PayloadBuildContext>,
    ) -> Release {
        let mut release = Release::default();
        let mut kept = Vec::with_capacity(self.ready.len());
        for held in self.ready.drain(..) {
            if held.context.view > new_view {
                kept.push(held);
            } else if held.context.view == new_view && Some(held.context) == required {
                release.released = Some(held);
            } else {
                release.discarded.push(held);
            }
        }
        self.ready = kept;
        release
    }

    /// On entering `new_view`: the held reports of own blocks whose child can
    /// now be built (`depth` views from the current one), sealed reports
    /// first. Reports of past views are dropped; later ones stay held.
    pub(super) fn take_reachable(&mut self, new_view: u64, depth: u64) -> Vec<PayloadBuildReady> {
        let reach = new_view + depth.max(1);
        let mut taken = Vec::new();
        for list in [&mut self.sealed, &mut self.validated] {
            list.retain(|held| held.context.view >= new_view);
            list.sort_by_key(|held| held.context.view);
            let mut later = Vec::new();
            for held in list.drain(..) {
                if held.context.view < reach {
                    taken.push(held);
                } else {
                    later.push(held);
                }
            }
            *list = later;
        }
        taken
    }

    /// Forgets the own block `block` that will not be proposed, and every
    /// record of a block built on it: held payloads, held reports and output
    /// builds. Builds still running keep their guards until they complete;
    /// what they produce is held and then dropped in its view.
    pub(super) fn drop_block(&mut self, block: B256) -> usize {
        let mut dropped = vec![block];
        let mut removed = 0;
        let mut at = 0;
        while at < dropped.len() {
            let gone = dropped[at];
            at += 1;
            for list in [&mut self.sealed, &mut self.validated] {
                let before = list.len();
                list.retain(|held| held.block_hash != gone);
                removed += before - list.len();
            }
            let before = self.output_builds.len();
            self.output_builds
                .retain(|build| build.context.parent_hash != gone);
            removed += before - self.output_builds.len();
            let mut kept = Vec::with_capacity(self.ready.len());
            for held in self.ready.drain(..) {
                if held.context.parent_hash == gone {
                    dropped.push(held.block_hash);
                    removed += 1;
                } else {
                    kept.push(held);
                }
            }
            self.ready = kept;
            for list in [&mut self.sealed, &mut self.validated] {
                for held in list.iter() {
                    if held.context.parent_hash == gone && !dropped.contains(&held.block_hash) {
                        dropped.push(held.block_hash);
                    }
                }
            }
        }
        removed
    }
}

/// How many execution-layer tips to keep: one when building a single block
/// ahead, as before; enough for a chain of own blocks when building two
/// ahead or when `chain` (`N42_FINALIZE_ON_TIP_CHAIN`) asks for it.
pub(super) fn tip_capacity(depth: u64, chain: bool) -> usize {
    if depth > 1 || chain { TIPS } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::super::ReadyKind;
    use super::*;

    fn context(view: u64, parent: u8) -> PayloadBuildContext {
        PayloadBuildContext {
            view,
            parent_hash: B256::repeat_byte(parent),
        }
    }

    fn report(kind: ReadyKind, view: u64, parent: u8, block: u8) -> PayloadBuildReady {
        PayloadBuildReady {
            context: context(view, parent),
            block_hash: B256::repeat_byte(block),
            kind,
        }
    }

    fn payload(view: u64, parent: u8, block: u8) -> PayloadBuildReady {
        report(ReadyKind::Payload, view, parent, block)
    }

    #[test]
    fn two_views_ahead_keep_their_own_records() {
        let mut ahead = AheadState::default();
        // Block 11 of view 6 on block 10, block 12 of view 7 on block 11.
        ahead.start_building(context(6, 10));
        ahead.start_building(context(7, 11));
        assert_eq!(ahead.building_on(B256::repeat_byte(10)), Some(context(6, 10)));
        assert_eq!(ahead.building_on(B256::repeat_byte(11)), Some(context(7, 11)));
        ahead.end_building(context(6, 10));
        assert_eq!(ahead.building_on(B256::repeat_byte(10)), None);
        assert_eq!(ahead.building_on(B256::repeat_byte(11)), Some(context(7, 11)));

        ahead.start_emitting(context(6, 10));
        ahead.start_emitting(context(7, 11));
        ahead.end_emitting(context(7, 11));
        assert!(ahead.is_emitting(context(6, 10)), "a later view's end keeps an earlier view's guard");
        assert!(!ahead.is_emitting(context(7, 11)));

        ahead.hold_payload(payload(6, 10, 11));
        ahead.hold_payload(payload(7, 11, 12));
        assert_eq!(ahead.ready.len(), 2);
        ahead.hold_payload(payload(7, 11, 13));
        assert_eq!(ahead.ready.len(), 2, "one payload per view");

        ahead.start_output_build(context(7, 11));
        ahead.start_output_build(context(8, 12));
        assert!(ahead.output_build(context(7, 11)).is_some());
        assert_eq!(ahead.take_output_build(context(8, 12)).map(|b| b.context), Some(context(8, 12)));
        assert!(ahead.output_build(context(7, 11)).is_some(), "another build's end keeps the record");

        ahead.clear();
        assert!(ahead.ready.is_empty() && ahead.building.is_empty() && ahead.emitting.is_empty());
        assert!(ahead.output_builds.is_empty());
    }

    #[test]
    fn a_view_releases_only_its_own_payload_under_its_locked_parent() {
        let mut ahead = AheadState::default();
        ahead.hold_payload(payload(6, 10, 11));
        ahead.hold_payload(payload(7, 11, 12));
        let release = ahead.take_release(6, Some(context(6, 10)));
        assert_eq!(release.released, Some(payload(6, 10, 11)));
        assert!(release.discarded.is_empty());
        assert_eq!(ahead.ready, vec![payload(7, 11, 12)], "the next view's payload waits for its view");

        // View 7 starts under a LockedQC that does not name block 11.
        let release = ahead.take_release(7, Some(context(7, 99)));
        assert_eq!(release.released, None);
        assert_eq!(release.discarded, vec![payload(7, 11, 12)]);
        assert!(ahead.ready.is_empty());

        // A view jump: what was held for a skipped view is discarded.
        ahead.hold_payload(payload(8, 12, 13));
        ahead.hold_payload(payload(9, 13, 14));
        let release = ahead.take_release(9, Some(context(9, 13)));
        assert_eq!(release.released, Some(payload(9, 13, 14)));
        assert_eq!(release.discarded, vec![payload(8, 12, 13)]);
    }

    #[test]
    fn a_dropped_block_takes_every_block_built_on_it() {
        let mut ahead = AheadState::default();
        // Block 11 (view 6) on 10, block 12 (view 7) on 11, an unrelated 21 on 20.
        ahead.hold_payload(payload(7, 11, 12));
        ahead.hold_payload(payload(8, 20, 21));
        ahead.sealed.push(report(ReadyKind::OwnBlockSealed, 7, 11, 12));
        ahead.validated.push(report(ReadyKind::OwnBlockValidated, 6, 10, 11));
        ahead.start_output_build(context(8, 12));
        ahead.start_output_build(context(7, 11));
        let removed = ahead.drop_block(B256::repeat_byte(11));
        assert_eq!(removed, 5);
        assert_eq!(ahead.ready, vec![payload(8, 20, 21)]);
        assert!(ahead.sealed.is_empty() && ahead.validated.is_empty());
        assert!(ahead.output_builds.is_empty());
    }

    #[test]
    fn held_reports_are_taken_once_within_reach() {
        let mut ahead = AheadState::default();
        ahead.validated.push(report(ReadyKind::OwnBlockValidated, 7, 11, 12));
        ahead.sealed.push(report(ReadyKind::OwnBlockSealed, 7, 11, 12));
        ahead.sealed.push(report(ReadyKind::OwnBlockSealed, 5, 9, 10));
        // One block ahead: only the view that starts.
        assert!(ahead.take_reachable(6, 1).is_empty());
        assert_eq!(ahead.sealed.len(), 1, "the past view's report is dropped");
        // Two blocks ahead: the view after the one that starts, sealed first.
        let taken = ahead.take_reachable(6, 2);
        assert_eq!(
            taken.iter().map(|held| held.kind).collect::<Vec<_>>(),
            vec![ReadyKind::OwnBlockSealed, ReadyKind::OwnBlockValidated]
        );
        assert!(ahead.sealed.is_empty() && ahead.validated.is_empty());
    }

    #[test]
    fn the_finalized_head_follows_the_own_chain_and_never_goes_back() {
        let hash = B256::repeat_byte;
        let mut ahead = AheadState::default();
        // Own blocks 11 on 10 and 12 on 11 accepted ahead of their commits.
        ahead.record_tip(hash(11), hash(10), tip_capacity(2, false));
        ahead.record_tip(hash(12), hash(11), tip_capacity(2, false));
        assert_eq!(ahead.head_for(hash(10)), hash(12), "not back to 11 or 10");
        assert_eq!(ahead.head_for(hash(11)), hash(12));
        assert_eq!(ahead.head_for(hash(12)), hash(12));
        assert_eq!(ahead.head_for(hash(30)), hash(30), "another chain");

        // One block ahead keeps one tip, as before.
        let mut single = AheadState::default();
        single.record_tip(hash(11), hash(10), tip_capacity(1, false));
        single.record_tip(hash(12), hash(11), tip_capacity(1, false));
        assert_eq!(single.tips.len(), 1);
        assert_eq!(single.head_for(hash(11)), hash(12));
        // The finalize of 10 then names 10 itself, an ancestor of the head 12:
        // the engine leaves its head and does not move finalized.
        assert_eq!(single.head_for(hash(10)), hash(10));
        let mut chained = AheadState::default();
        chained.record_tip(hash(11), hash(10), tip_capacity(1, true));
        chained.record_tip(hash(12), hash(11), tip_capacity(1, true));
        assert_eq!(chained.head_for(hash(10)), hash(12));

        // The capacity bounds the chain.
        for n in 13..20 {
            ahead.record_tip(hash(n), hash(n - 1), tip_capacity(2, false));
        }
        assert_eq!(ahead.tips.len(), TIPS);
        assert_eq!(ahead.head_for(hash(15)), hash(19));
    }

    #[test]
    fn a_child_is_covered_by_any_build_of_it() {
        let child = context(7, 11);
        let mut ahead = AheadState::default();
        assert!(!ahead.child_covered(child));
        ahead.start_output_build(child);
        assert!(ahead.child_covered(child));
        ahead.take_output_build(child);
        ahead.hold_payload(payload(7, 11, 12));
        assert!(ahead.child_covered(child), "produced and held for its view");
        ahead.ready.clear();
        ahead.start_emitting(child);
        assert!(ahead.child_covered(child), "on its way to its proposal");
        ahead.end_emitting(child);
        ahead.start_building(child);
        assert!(ahead.child_covered(child), "built through a forkchoice update");
        assert!(!ahead.child_covered(context(7, 12)));
    }
}
