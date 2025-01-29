use std::{
    fmt::{Debug, Display, Formatter},
    iter,
    num::NonZeroU32,
    ops::ControlFlow,
};

use elsa::FrozenMap;

use crate::{
    internal::{
        arena::{Arena, ArenaId},
        id::{ClauseId, LearntClauseId, StringId, VersionSetId},
    },
    solver::{
        decision_map::DecisionMap, decision_tracker::DecisionTracker, variable_map::VariableMap,
        VariableId,
    },
    Interner, NameId, Requirement,
};

/// Represents a single clause in the SAT problem
///
/// # SAT terminology
///
/// Clauses consist of disjunctions of literals (i.e. a non-empty list of
/// variables, potentially negated, joined by the logical "or" operator). Here
/// are some examples:
///
/// - (¬A ∨ ¬B)
/// - (¬A ∨ ¬B ∨ ¬C ∨ ¬D)
/// - (¬A ∨ B ∨ C)
/// - (root)
///
/// For additional clarity: if `(¬A ∨ ¬B)` is a clause, `¬A` and `¬B` are its
/// literals, and `A` and `B` are variables. In our implementation, variables
/// are represented by [`VariableId`], and assignments are tracked in
/// the [`DecisionMap`].
///
/// The solver will attempt to assign values to the variables involved in the
/// problem in such a way that all clauses become true. If that turns out to be
/// impossible, the problem is unsatisfiable.
///
/// Since we are not interested in general-purpose SAT solving, but are
/// targeting the specific use-case of dependency resolution, we only support a
/// limited set of clauses. There are thousands of clauses for a particular
/// dependency resolution problem, and we try to keep the [`Clause`] enum small.
/// A naive implementation would store a `Vec<Literal>`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Clause {
    /// An assertion that the root solvable must be installed
    ///
    /// In SAT terms: (root)
    InstallRoot,
    /// Makes the solvable require the candidates associated with the
    /// [`Requirement`].
    ///
    /// In SAT terms: (¬A ∨ B1 ∨ B2 ∨ ... ∨ B99), where B1 to B99 represent the
    /// possible candidates for the provided [`Requirement`].
    Requires(VariableId, Requirement),
    /// Ensures only a single version of a package is installed
    ///
    /// Usage: generate one [`Clause::ForbidMultipleInstances`] clause for each
    /// possible combination of packages under the same name. The clause
    /// itself forbids two solvables from being installed at the same time.
    ///
    /// In SAT terms: (¬A ∨ ¬B)
    ForbidMultipleInstances(VariableId, Literal, NameId),
    /// Forbids packages that do not satisfy a solvable's constrains
    ///
    /// Usage: for each constrains relationship in a package, determine all the
    /// candidates that do _not_ satisfy it, and create one
    /// [`Clause::Constrains`]. The clause itself forbids two solvables from
    /// being installed at the same time, just as
    /// [`Clause::ForbidMultipleInstances`], but it pays off to have a
    /// separate variant for user-friendly error messages.
    ///
    /// In SAT terms: (¬A ∨ ¬B)
    Constrains(VariableId, VariableId, VersionSetId),
    /// In SAT terms: (¬A ∨ ¬C ∨ B1 ∨ B2 ∨ ... ∨ B99), where A is the solvable,
    /// C is the condition, and B1 to B99 represent the possible candidates for
    /// the provided [`Requirement`].
    /// We need to store the version set id because in the conflict graph, the version set id
    /// is used to identify the condition variable.
    Conditional(VariableId, VariableId, VersionSetId, Requirement),
    /// A conditional clause that requires a feature to be enabled for the requirement to be active.
    ///
    /// In SAT terms: (¬A ∨ ¬C ∨ ¬F ∨ B1 ∨ B2 ∨ ... ∨ B99), where A is the solvable,
    /// C is the condition, F is the feature, and B1 to B99 represent the possible candidates for
    /// the provided [`Requirement`].
    ConditionalWithExtra(
        VariableId,   // solvable
        VariableId,   // condition
        VersionSetId, // condition version set
        VariableId,   // extra
        StringId,     // extra name
        Requirement,  // requirement
    ),
    /// A requirement that requires a feature to be enabled for the requirement to be active.
    ///
    /// In SAT terms: (¬A ∨ ¬F ∨ B1 ∨ B2 ∨ ... ∨ B99), where A is the solvable,
    /// F is the feature, and B1 to B99 represent the possible candidates for
    /// the provided [`Requirement`].
    RequiresWithExtra(VariableId, VariableId, StringId, Requirement),
    /// Forbids the package on the right-hand side
    ///
    /// Note that the package on the left-hand side is not part of the clause,
    /// but just context to know which exact package was locked (necessary
    /// for user-friendly error messages)
    ///
    /// In SAT terms: (¬root ∨ ¬B). Note that we could encode this as an
    /// assertion (¬B), but that would require additional logic in the
    /// solver.
    Lock(VariableId, VariableId),
    /// A clause learnt during solving
    ///
    /// The learnt clause id can be used to retrieve the clause's literals,
    /// which are stored elsewhere to prevent the size of [`Clause`] from
    /// blowing up
    Learnt(LearntClauseId),

    /// A clause that forbids a package from being installed for an external
    /// reason.
    Excluded(VariableId, StringId),
}

impl Clause {
    /// Returns the building blocks needed for a new [WatchedLiterals] of the
    /// [Clause::Requires] kind.
    ///
    /// These building blocks are:
    ///
    /// - The [Clause] itself;
    /// - The ids of the solvables that will be watched (unless there are no
    ///   candidates, i.e. the clause is actually an assertion);
    /// - A boolean indicating whether the clause conflicts with existing
    ///   decisions. This should always be false when adding clauses before
    ///   starting the solving process, but can be true for clauses that are
    ///   added dynamically.
    fn requires(
        parent: VariableId,
        requirement: Requirement,
        candidates: impl IntoIterator<Item = VariableId>,
        decision_tracker: &DecisionTracker,
    ) -> (Self, Option<[Literal; 2]>, bool) {
        // It only makes sense to introduce a requires clause when the parent solvable
        // is undecided or going to be installed
        assert_ne!(decision_tracker.assigned_value(parent), Some(false));

        let kind = Clause::Requires(parent, requirement);
        let mut candidates = candidates.into_iter().peekable();
        let first_candidate = candidates.peek().copied();
        if let Some(first_candidate) = first_candidate {
            match candidates.find(|&c| decision_tracker.assigned_value(c) != Some(false)) {
                // Watch any candidate that is not assigned to false
                Some(watched_candidate) => (
                    kind,
                    Some([parent.negative(), watched_candidate.positive()]),
                    false,
                ),

                // All candidates are assigned to false! Therefore, the clause conflicts with the
                // current decisions. There are no valid watches for it at the moment, but we will
                // assign default ones nevertheless, because they will become valid after the solver
                // restarts.
                None => (
                    kind,
                    Some([parent.negative(), first_candidate.positive()]),
                    true,
                ),
            }
        } else {
            // If there are no candidates there is no need to watch anything.
            (kind, None, false)
        }
    }

    /// Returns the building blocks needed for a new [WatchedLiterals] of the
    /// [Clause::Constrains] kind.
    ///
    /// These building blocks are:
    ///
    /// - The [Clause] itself;
    /// - The ids of the solvables that will be watched;
    /// - A boolean indicating whether the clause conflicts with existing
    ///   decisions. This should always be false when adding clauses before
    ///   starting the solving process, but can be true for clauses that are
    ///   added dynamically.
    fn constrains(
        parent: VariableId,
        forbidden_solvable: VariableId,
        via: VersionSetId,
        decision_tracker: &DecisionTracker,
    ) -> (Self, Option<[Literal; 2]>, bool) {
        // It only makes sense to introduce a constrains clause when the parent solvable
        // is undecided or going to be installed
        assert_ne!(decision_tracker.assigned_value(parent), Some(false));

        // If the forbidden solvable is already assigned to true, that means that there
        // is a conflict with current decisions, because it implies the parent
        // solvable would be false (and we just asserted that it is not)
        let conflict = decision_tracker.assigned_value(forbidden_solvable) == Some(true);

        (
            Clause::Constrains(parent, forbidden_solvable, via),
            Some([parent.negative(), forbidden_solvable.negative()]),
            conflict,
        )
    }

    /// Returns the ids of the solvables that will be watched as well as the
    /// clause itself.
    fn forbid_multiple(
        candidate: VariableId,
        constrained_candidate: Literal,
        name: NameId,
    ) -> (Self, Option<[Literal; 2]>) {
        (
            Clause::ForbidMultipleInstances(candidate, constrained_candidate, name),
            Some([candidate.negative(), constrained_candidate]),
        )
    }

    fn root() -> (Self, Option<[Literal; 2]>) {
        (Clause::InstallRoot, None)
    }

    fn exclude(candidate: VariableId, reason: StringId) -> (Self, Option<[Literal; 2]>) {
        (Clause::Excluded(candidate, reason), None)
    }

    fn lock(
        locked_candidate: VariableId,
        other_candidate: VariableId,
    ) -> (Self, Option<[Literal; 2]>) {
        (
            Clause::Lock(locked_candidate, other_candidate),
            Some([VariableId::root().negative(), other_candidate.negative()]),
        )
    }

    fn learnt(
        learnt_clause_id: LearntClauseId,
        literals: &[Literal],
    ) -> (Self, Option<[Literal; 2]>) {
        debug_assert!(!literals.is_empty());
        (
            Clause::Learnt(learnt_clause_id),
            if literals.len() == 1 {
                // No need for watches, since we learned an assertion
                None
            } else {
                Some([*literals.first().unwrap(), *literals.last().unwrap()])
            },
        )
    }

    fn conditional(
        parent_id: VariableId,
        requirement: Requirement,
        condition_variable: VariableId,
        condition_version_set_id: VersionSetId,
        decision_tracker: &DecisionTracker,
        requirement_candidates: impl IntoIterator<Item = VariableId>,
    ) -> (Self, Option<[Literal; 2]>, bool) {
        assert_ne!(decision_tracker.assigned_value(parent_id), Some(false));
        let mut requirement_candidates = requirement_candidates.into_iter();

        let requirement_literal =
            if decision_tracker.assigned_value(condition_variable) == Some(true) {
                // then ~condition is false
                requirement_candidates
                    .find(|&id| decision_tracker.assigned_value(id) != Some(false))
                    .map(|id| id.positive())
            } else {
                None
            };

        (
            Clause::Conditional(
                parent_id,
                condition_variable,
                condition_version_set_id,
                requirement,
            ),
            Some([
                parent_id.negative(),
                requirement_literal.unwrap_or(condition_variable.negative()),
            ]),
            requirement_literal.is_none()
                && decision_tracker.assigned_value(condition_variable) == Some(true),
        )
    }

    fn conditional_with_extra(
        parent_id: VariableId,
        requirement: Requirement,
        condition_variable: VariableId,
        condition_version_set_id: VersionSetId,
        extra_variable: VariableId,
        extra_name: StringId,
        decision_tracker: &DecisionTracker,
        requirement_candidates: impl IntoIterator<Item = VariableId>,
    ) -> (Self, Option<[Literal; 2]>, bool) {
        assert_ne!(decision_tracker.assigned_value(parent_id), Some(false));
        let mut requirement_candidates = requirement_candidates.into_iter();

        let requirement_literal =
            if decision_tracker.assigned_value(condition_variable) == Some(true) {
                // then ~condition is false
                // if the feature is enabled, then we need to watch the requirement candidates
                if decision_tracker.assigned_value(extra_variable) == Some(true) {
                    requirement_candidates
                        .find(|&id| decision_tracker.assigned_value(id) != Some(false))
                        .map(|id| id.positive())
                } else {
                    // if the feature is disabled, then we need to watch the feature variable
                    Some(extra_variable.negative())
                }
            } else {
                None
            };
        (
            Clause::ConditionalWithExtra(
                parent_id,
                condition_variable,
                condition_version_set_id,
                extra_variable,
                extra_name,
                requirement,
            ),
            Some([
                parent_id.negative(),
                requirement_literal.unwrap_or(condition_variable.negative()),
            ]),
            requirement_literal.is_none()
                && decision_tracker.assigned_value(condition_variable) == Some(true)
                && decision_tracker.assigned_value(extra_variable) == Some(true),
        )
    }

    fn requires_with_extra(
        solvable_id: VariableId,
        extra_variable: VariableId,
        extra_name: StringId,
        requirement: Requirement,
        decision_tracker: &DecisionTracker,
        requirement_candidates: impl IntoIterator<Item = VariableId>,
    ) -> (Self, Option<[Literal; 2]>, bool) {
        // It only makes sense to introduce a requires clause when the parent solvable
        // is undecided or going to be installed
        assert_ne!(decision_tracker.assigned_value(solvable_id), Some(false));

        let kind = Clause::RequiresWithExtra(solvable_id, extra_variable, extra_name, requirement);
        let mut candidates = requirement_candidates.into_iter().peekable();
        let first_candidate = candidates.peek().copied();

        if decision_tracker.assigned_value(extra_variable) == Some(true) {
            // Feature is enabled, so watch the requirement candidates
            if let Some(first_candidate) = first_candidate {
                match candidates.find(|&c| decision_tracker.assigned_value(c) != Some(false)) {
                    // Watch any candidate that is not assigned to false
                    Some(watched_candidate) => (
                        kind,
                        Some([solvable_id.negative(), watched_candidate.positive()]),
                        false,
                    ),
                    // All candidates are assigned to false - conflict
                    None => (
                        kind,
                        Some([solvable_id.negative(), first_candidate.positive()]),
                        true,
                    ),
                }
            } else {
                // No candidates available
                (kind, None, false)
            }
        } else {
            // Feature is not enabled, so watch the feature variable
            (
                kind,
                Some([solvable_id.negative(), extra_variable.negative()]),
                false,
            )
        }
    }

    /// Tries to fold over all the literals in the clause.
    ///
    /// This function is useful to iterate, find, or filter the literals in a
    /// clause.
    pub fn try_fold_literals<B, C, F>(
        &self,
        learnt_clauses: &Arena<LearntClauseId, Vec<Literal>>,
        requirements_to_sorted_candidates: &FrozenMap<
            Requirement,
            Vec<Vec<VariableId>>,
            ahash::RandomState,
        >,
        init: C,
        mut visit: F,
    ) -> ControlFlow<B, C>
    where
        F: FnMut(C, Literal) -> ControlFlow<B, C>,
    {
        match *self {
            Clause::InstallRoot => unreachable!(),
            Clause::Excluded(solvable, _) => visit(init, solvable.negative()),
            Clause::Learnt(learnt_id) => learnt_clauses[learnt_id]
                .iter()
                .copied()
                .try_fold(init, visit),
            Clause::Requires(solvable_id, match_spec_id) => iter::once(solvable_id.negative())
                .chain(
                    requirements_to_sorted_candidates[&match_spec_id]
                        .iter()
                        .flatten()
                        .map(|&s| s.positive()),
                )
                .try_fold(init, visit),
            Clause::Constrains(s1, s2, _) => [s1.negative(), s2.negative()]
                .into_iter()
                .try_fold(init, visit),
            Clause::ForbidMultipleInstances(s1, s2, _) => {
                [s1.negative(), s2].into_iter().try_fold(init, visit)
            }
            Clause::Lock(_, s) => [s.negative(), VariableId::root().negative()]
                .into_iter()
                .try_fold(init, visit),
            Clause::Conditional(package_id, condition_variable, _, requirement) => {
                iter::once(package_id.negative())
                    .chain(iter::once(condition_variable.negative()))
                    .chain(
                        requirements_to_sorted_candidates[&requirement]
                            .iter()
                            .flatten()
                            .map(|&s| s.positive()),
                    )
                    .try_fold(init, visit)
            }
            Clause::ConditionalWithExtra(
                package_id,
                condition_variable,
                _,
                extra_variable,
                _,
                requirement,
            ) => iter::once(package_id.negative())
                .chain(iter::once(condition_variable.negative()))
                .chain(iter::once(extra_variable.negative()))
                .chain(
                    requirements_to_sorted_candidates[&requirement]
                        .iter()
                        .flatten()
                        .map(|&s| s.positive()),
                )
                .try_fold(init, visit),
            Clause::RequiresWithExtra(solvable_id, extra_variable, _, requirement) => {
                iter::once(solvable_id.negative())
                    .chain(iter::once(solvable_id.negative()))
                    .chain(iter::once(extra_variable.negative()))
                    .chain(
                        requirements_to_sorted_candidates[&requirement]
                            .iter()
                            .flatten()
                            .map(|&s| s.positive()),
                    )
                    .try_fold(init, visit)
            }
        }
    }

    /// Visits each literal in the clause.
    ///
    /// If you need to exit early or return a value, use [`try_fold_literals`].
    pub fn visit_literals(
        &self,
        learnt_clauses: &Arena<LearntClauseId, Vec<Literal>>,
        requirements_to_sorted_candidates: &FrozenMap<
            Requirement,
            Vec<Vec<VariableId>>,
            ahash::RandomState,
        >,
        mut visit: impl FnMut(Literal),
    ) {
        self.try_fold_literals(
            learnt_clauses,
            requirements_to_sorted_candidates,
            (),
            |_, lit| {
                visit(lit);
                ControlFlow::<()>::Continue(())
            },
        );
    }

    /// Construct a [`ClauseDisplay`] to display the clause.
    pub fn display<'i, I: Interner>(
        &self,
        variable_map: &'i VariableMap,
        interner: &'i I,
    ) -> ClauseDisplay<'i, I> {
        ClauseDisplay {
            kind: *self,
            variable_map,
            interner,
        }
    }
}

/// Keeps track of the literals watched by a [`Clause`] and the state associated
/// to two linked lists this clause is part of
///
/// In our SAT implementation, each clause tracks two literals present in its
/// clause, to be notified when the value assigned to the variable has changed
/// (this technique is known as _watches_). Clauses that are tracking the same
/// variable are grouped together in a linked list, so it becomes easy to notify
/// them all.
#[derive(Clone)]
pub(crate) struct WatchedLiterals {
    /// The ids of the literals this clause is watching. A clause that is
    /// watching literals is always watching two literals, no more, no less.
    pub watched_literals: [Literal; 2],
    /// The ids of the next clause in each linked list that this clause is part
    /// of. If either of these or `None` then there is no next clause.
    pub(crate) next_watches: [Option<ClauseId>; 2],
}

impl WatchedLiterals {
    /// Shorthand method to construct a [`Clause::InstallRoot`] without
    /// requiring complicated arguments.
    pub fn root() -> (Option<Self>, Clause) {
        let (kind, watched_literals) = Clause::root();
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    /// Shorthand method to construct a [Clause::Requires] without requiring
    /// complicated arguments.
    ///
    /// The returned boolean value is true when adding the clause resulted in a
    /// conflict.
    pub fn requires(
        candidate: VariableId,
        requirement: Requirement,
        matching_candidates: impl IntoIterator<Item = VariableId>,
        decision_tracker: &DecisionTracker,
    ) -> (Option<Self>, bool, Clause) {
        let (kind, watched_literals, conflict) = Clause::requires(
            candidate,
            requirement,
            matching_candidates,
            decision_tracker,
        );

        (
            Self::from_kind_and_initial_watches(watched_literals),
            conflict,
            kind,
        )
    }

    /// Shorthand method to construct a [Clause::Constrains] without requiring
    /// complicated arguments.
    ///
    /// The returned boolean value is true when adding the clause resulted in a
    /// conflict.
    pub fn constrains(
        candidate: VariableId,
        constrained_package: VariableId,
        requirement: VersionSetId,
        decision_tracker: &DecisionTracker,
    ) -> (Option<Self>, bool, Clause) {
        let (kind, watched_literals, conflict) = Clause::constrains(
            candidate,
            constrained_package,
            requirement,
            decision_tracker,
        );

        (
            Self::from_kind_and_initial_watches(watched_literals),
            conflict,
            kind,
        )
    }

    pub fn lock(
        locked_candidate: VariableId,
        other_candidate: VariableId,
    ) -> (Option<Self>, Clause) {
        let (kind, watched_literals) = Clause::lock(locked_candidate, other_candidate);
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    pub fn forbid_multiple(
        candidate: VariableId,
        other_candidate: Literal,
        name: NameId,
    ) -> (Option<Self>, Clause) {
        let (kind, watched_literals) = Clause::forbid_multiple(candidate, other_candidate, name);
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    pub fn learnt(
        learnt_clause_id: LearntClauseId,
        literals: &[Literal],
    ) -> (Option<Self>, Clause) {
        let (kind, watched_literals) = Clause::learnt(learnt_clause_id, literals);
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    pub fn exclude(candidate: VariableId, reason: StringId) -> (Option<Self>, Clause) {
        let (kind, watched_literals) = Clause::exclude(candidate, reason);
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    /// Shorthand method to construct a [Clause::Conditional] without requiring
    /// complicated arguments.
    ///
    /// The returned boolean value is true when adding the clause resulted in a
    /// conflict.
    pub fn conditional(
        package_id: VariableId,
        requirement: Requirement,
        condition_variable: VariableId,
        condition_version_set_id: VersionSetId,
        decision_tracker: &DecisionTracker,
        requirement_candidates: impl IntoIterator<Item = VariableId>,
    ) -> (Option<Self>, bool, Clause) {
        let (kind, watched_literals, conflict) = Clause::conditional(
            package_id,
            requirement,
            condition_variable,
            condition_version_set_id,
            decision_tracker,
            requirement_candidates,
        );

        (
            WatchedLiterals::from_kind_and_initial_watches(watched_literals),
            conflict,
            kind,
        )
    }

    /// Shorthand method to construct a [Clause::ConditionalWithExtra] without requiring
    /// complicated arguments.
    ///
    /// The returned boolean value is true when adding the clause resulted in a
    /// conflict.
    pub fn conditional_with_extra(
        package_id: VariableId,
        requirement: Requirement,
        condition_variable: VariableId,
        condition_version_set_id: VersionSetId,
        extra_variable: VariableId,
        extra_name: StringId,
        decision_tracker: &DecisionTracker,
        requirement_candidates: impl IntoIterator<Item = VariableId>,
    ) -> (Option<Self>, bool, Clause) {
        let (kind, watched_literals, conflict) = Clause::conditional_with_extra(
            package_id,
            requirement,
            condition_variable,
            condition_version_set_id,
            extra_variable,
            extra_name,
            decision_tracker,
            requirement_candidates,
        );
        (
            WatchedLiterals::from_kind_and_initial_watches(watched_literals),
            conflict,
            kind,
        )
    }

    /// Shorthand method to construct a [Clause::RequiresWithExtra] without requiring
    /// complicated arguments.
    ///
    /// The returned boolean value is true when adding the clause resulted in a
    /// conflict.
    pub fn requires_with_extra(
        solvable_id: VariableId,
        extra_variable: VariableId,
        extra_name: StringId,
        requirement: Requirement,
        decision_tracker: &DecisionTracker,
        requirement_candidates: impl IntoIterator<Item = VariableId>,
    ) -> (Option<Self>, bool, Clause) {
        let (kind, watched_literals, conflict) = Clause::requires_with_extra(
            solvable_id,
            extra_variable,
            extra_name,
            requirement,
            decision_tracker,
            requirement_candidates,
        );
        (
            WatchedLiterals::from_kind_and_initial_watches(watched_literals),
            conflict,
            kind,
        )
    }

    fn from_kind_and_initial_watches(watched_literals: Option<[Literal; 2]>) -> Option<Self> {
        let watched_literals = watched_literals?;
        debug_assert!(watched_literals[0] != watched_literals[1]);
        Some(Self {
            watched_literals,
            next_watches: [None, None],
        })
    }

    pub fn next_unwatched_literal(
        &self,
        clause: &Clause,
        learnt_clauses: &Arena<LearntClauseId, Vec<Literal>>,
        requirement_to_sorted_candidates: &FrozenMap<
            Requirement,
            Vec<Vec<VariableId>>,
            ahash::RandomState,
        >,
        decision_map: &DecisionMap,
        for_watch_index: usize,
    ) -> Option<Literal> {
        let other_watch_index = 1 - for_watch_index;

        match clause {
            Clause::InstallRoot => unreachable!(),
            Clause::Excluded(_, _) => unreachable!(),
            Clause::Constrains(..) | Clause::ForbidMultipleInstances(..) | Clause::Lock(..) => {
                // We cannot move the watches in these clauses.
                None
            }
            clause => {
                let next = clause.try_fold_literals(
                    learnt_clauses,
                    requirement_to_sorted_candidates,
                    (),
                    |_, lit| {
                        // The next unwatched variable (if available), is a variable that is:
                        // * Not already being watched
                        // * Not yet decided, or decided in such a way that the literal yields true
                        if self.watched_literals[other_watch_index] != lit
                            && lit.eval(decision_map).unwrap_or(true)
                        {
                            ControlFlow::Break(lit)
                        } else {
                            ControlFlow::Continue(())
                        }
                    },
                );
                match next {
                    ControlFlow::Break(lit) => Some(lit),
                    ControlFlow::Continue(_) => None,
                }
            }
        }
    }
}

/// Represents a literal in a SAT clause, a literal holds a variable and
/// indicates whether it should be positive or negative (i.e. either A or ¬A).
///
/// A [`Literal`] stores a [`NonZeroU32`] which ensures that the size of an
/// `Option<Literal>` is the same as a `Literal`.
#[repr(transparent)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) struct Literal(NonZeroU32);

impl Literal {
    /// Constructs a new [`Literal`] from a [`VariableId`] and a boolean
    /// indicating whether the literal should be negated.
    pub fn new(variable: VariableId, negate: bool) -> Self {
        let variable_idx = variable.to_usize();
        let encoded_literal = variable_idx << 1 | negate as usize;
        Self::from_usize(encoded_literal)
    }
}

impl ArenaId for Literal {
    fn from_usize(x: usize) -> Self {
        let idx: u32 = (x + 1).try_into().expect("watched literal id too big");
        // SAFETY: This is safe because we are adding 1 to the index
        unsafe { Literal(NonZeroU32::new_unchecked(idx)) }
    }

    fn to_usize(self) -> usize {
        self.0.get() as usize - 1
    }
}

impl Literal {
    pub fn negate(&self) -> bool {
        (self.0.get() & 1) == 0
    }

    /// Returns the value that would make the literal evaluate to true if
    /// assigned to the literal's solvable
    pub(crate) fn satisfying_value(self) -> bool {
        !self.negate()
    }

    /// Returns the value that would make the literal evaluate to true if
    /// assigned to the literal's solvable
    #[inline]
    pub(crate) fn variable(self) -> VariableId {
        VariableId::from_usize(self.to_usize() >> 1)
    }

    /// Evaluates the literal, or returns `None` if no value has been assigned
    /// to the solvable
    #[inline(always)]
    pub(crate) fn eval(self, decision_map: &DecisionMap) -> Option<bool> {
        decision_map
            .value(self.variable())
            .map(|value| value != self.negate())
    }
}

impl VariableId {
    /// Constructs a [`Literal`] that indicates this solvable should be assigned
    /// a positive value.
    pub fn positive(self) -> Literal {
        Literal::new(self, false)
    }

    /// Constructs a [`Literal`] that indicates this solvable should be assigned
    /// a negative value.
    pub fn negative(self) -> Literal {
        Literal::new(self, true)
    }
}

/// A representation of a clause that implements [`Debug`]
pub(crate) struct ClauseDisplay<'i, I: Interner> {
    kind: Clause,
    interner: &'i I,
    variable_map: &'i VariableMap,
}

impl<'i, I: Interner> Display for ClauseDisplay<'i, I> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            Clause::InstallRoot => write!(f, "InstallRoot"),
            Clause::Excluded(variable, reason) => {
                write!(
                    f,
                    "Excluded({}({:?}), {})",
                    variable.display(self.variable_map, self.interner),
                    variable,
                    self.interner.display_string(reason)
                )
            }
            Clause::Learnt(learnt_id) => write!(f, "Learnt({learnt_id:?})"),
            Clause::Requires(variable, requirement) => {
                write!(
                    f,
                    "Requires({}({:?}), {})",
                    variable.display(self.variable_map, self.interner),
                    variable,
                    requirement.display(self.interner),
                )
            }
            Clause::Constrains(v1, v2, version_set_id) => {
                write!(
                    f,
                    "Constrains({}({:?}), {}({:?}), {})",
                    v1.display(self.variable_map, self.interner),
                    v1,
                    v2.display(self.variable_map, self.interner),
                    v2,
                    self.interner.display_version_set(version_set_id)
                )
            }
            Clause::ForbidMultipleInstances(v1, v2, name) => {
                write!(
                    f,
                    "ForbidMultipleInstances({}({:?}), {}({:?}), {})",
                    v1.display(self.variable_map, self.interner),
                    v1,
                    v2.variable().display(self.variable_map, self.interner),
                    v2,
                    self.interner.display_name(name)
                )
            }
            Clause::Lock(locked, other) => {
                write!(
                    f,
                    "Lock({}({:?}), {}({:?}))",
                    locked.display(self.variable_map, self.interner),
                    locked,
                    other.display(self.variable_map, self.interner),
                    other,
                )
            }
            Clause::Conditional(package_id, condition_variable, _, requirement) => {
                write!(
                    f,
                    "Conditional({}({:?}), {}({:?}), {})",
                    package_id.display(self.variable_map, self.interner),
                    package_id,
                    condition_variable.display(self.variable_map, self.interner),
                    condition_variable,
                    requirement.display(self.interner),
                )
            }
            Clause::ConditionalWithExtra(
                package_id,
                condition_variable,
                _,
                extra_variable,
                _,
                requirement,
            ) => {
                write!(
                    f,
                    "ConditionalWithExtra({}({:?}), {}({:?}), {}({:?}), {})",
                    package_id.display(self.variable_map, self.interner),
                    package_id,
                    condition_variable.display(self.variable_map, self.interner),
                    condition_variable,
                    extra_variable.display(self.variable_map, self.interner),
                    extra_variable,
                    requirement.display(self.interner),
                )
            }
            Clause::RequiresWithExtra(solvable_id, extra_variable, _, requirement) => {
                write!(
                    f,
                    "RequiresWithExtra({}({:?}), {}({:?}), {})",
                    solvable_id.display(self.variable_map, self.interner),
                    solvable_id,
                    extra_variable.display(self.variable_map, self.interner),
                    extra_variable,
                    requirement.display(self.interner),
                )
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{internal::arena::ArenaId, solver::decision::Decision};

    #[test]
    #[allow(clippy::bool_assert_comparison)]
    fn test_literal_satisfying_value() {
        let lit = VariableId::root().negative();
        assert_eq!(lit.satisfying_value(), false);

        let lit = VariableId::root().positive();
        assert_eq!(lit.satisfying_value(), true);
    }

    #[test]
    fn test_literal_eval() {
        let mut decision_map = DecisionMap::new();

        let literal = VariableId::root().positive();
        let negated_literal = VariableId::root().negative();

        // Undecided
        assert_eq!(literal.eval(&decision_map), None);
        assert_eq!(negated_literal.eval(&decision_map), None);

        // Decided
        decision_map.set(VariableId::root(), true, 1);
        assert_eq!(literal.eval(&decision_map), Some(true));
        assert_eq!(negated_literal.eval(&decision_map), Some(false));

        decision_map.set(VariableId::root(), false, 1);
        assert_eq!(literal.eval(&decision_map), Some(false));
        assert_eq!(negated_literal.eval(&decision_map), Some(true));
    }

    #[test]
    fn test_requires_with_and_without_conflict() {
        let mut decisions = DecisionTracker::new();

        let parent = VariableId::from_usize(1);
        let candidate1 = VariableId::from_usize(2);
        let candidate2 = VariableId::from_usize(3);

        // No conflict, all candidates available
        let (clause, conflict, _kind) = WatchedLiterals::requires(
            parent,
            VersionSetId::from_usize(0).into(),
            [candidate1, candidate2],
            &decisions,
        );
        assert!(!conflict);
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[0].variable(),
            parent
        );
        assert_eq!(clause.unwrap().watched_literals[1].variable(), candidate1);

        // No conflict, still one candidate available
        decisions
            .try_add_decision(Decision::new(candidate1, false, ClauseId::from_usize(0)), 1)
            .unwrap();
        let (clause, conflict, _kind) = WatchedLiterals::requires(
            parent,
            VersionSetId::from_usize(0).into(),
            [candidate1, candidate2],
            &decisions,
        );
        assert!(!conflict);
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[0].variable(),
            parent
        );
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[1].variable(),
            candidate2
        );

        // Conflict, no candidates available
        decisions
            .try_add_decision(
                Decision::new(candidate2, false, ClauseId::install_root()),
                1,
            )
            .unwrap();
        let (clause, conflict, _kind) = WatchedLiterals::requires(
            parent,
            VersionSetId::from_usize(0).into(),
            [candidate1, candidate2],
            &decisions,
        );
        assert!(conflict);
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[0].variable(),
            parent
        );
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[1].variable(),
            candidate1
        );

        // Panic
        decisions
            .try_add_decision(Decision::new(parent, false, ClauseId::install_root()), 1)
            .unwrap();
        let panicked = std::panic::catch_unwind(|| {
            WatchedLiterals::requires(
                parent,
                VersionSetId::from_usize(0).into(),
                [candidate1, candidate2],
                &decisions,
            )
        })
        .is_err();
        assert!(panicked);
    }

    #[test]
    fn test_constrains_with_and_without_conflict() {
        let mut decisions = DecisionTracker::new();

        let parent = VariableId::from_usize(1);
        let forbidden = VariableId::from_usize(2);

        // No conflict, forbidden package not installed
        let (clause, conflict, _kind) =
            WatchedLiterals::constrains(parent, forbidden, VersionSetId::from_usize(0), &decisions);
        assert!(!conflict);
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[0].variable(),
            parent
        );
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[1].variable(),
            forbidden
        );

        // Conflict, forbidden package installed
        decisions
            .try_add_decision(Decision::new(forbidden, true, ClauseId::install_root()), 1)
            .unwrap();
        let (clause, conflict, _kind) =
            WatchedLiterals::constrains(parent, forbidden, VersionSetId::from_usize(0), &decisions);
        assert!(conflict);
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[0].variable(),
            parent
        );
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[1].variable(),
            forbidden
        );

        // Panic
        decisions
            .try_add_decision(Decision::new(parent, false, ClauseId::install_root()), 1)
            .unwrap();
        let panicked = std::panic::catch_unwind(|| {
            WatchedLiterals::constrains(parent, forbidden, VersionSetId::from_usize(0), &decisions)
        })
        .is_err();
        assert!(panicked);
    }

    #[test]
    fn test_watched_literals_size() {
        // This test is here to ensure we don't increase the size of `WatchedLiterals`
        // by accident, as we are creating thousands of instances.
        // libsolv: 24 bytes
        assert_eq!(std::mem::size_of::<WatchedLiterals>(), 16);
    }

    #[test]
    fn test_literal_size() {
        assert_eq!(std::mem::size_of::<Literal>(), 4);
        assert_eq!(
            std::mem::size_of::<Literal>(),
            std::mem::size_of::<Option<Literal>>()
        );
        assert_eq!(
            std::mem::size_of::<Literal>() * 2,
            std::mem::size_of::<[Literal; 2]>()
        );
        assert_eq!(
            std::mem::size_of::<Literal>() * 2,
            std::mem::size_of::<[Option<Literal>; 2]>()
        );
        assert_eq!(
            std::mem::size_of::<Literal>() * 2,
            std::mem::size_of::<Option<[Literal; 2]>>()
        );
    }

    #[test]
    fn test_watched_literal_size() {
        assert_eq!(std::mem::size_of::<WatchedLiterals>(), 16);
        assert_eq!(
            std::mem::size_of::<Option<WatchedLiterals>>(),
            std::mem::size_of::<WatchedLiterals>()
        );
    }
}
