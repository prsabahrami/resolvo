use crate::{Interner, StringId, VersionSetId, VersionSetUnionId};
use itertools::Itertools;
use std::fmt::Display;

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Condition {
    /// A condition that must be met for the requirement to be active.
    VersionSetId(VersionSetId),
    /// An extra which if enabled, requires further dependencies to be met.
    Extra(StringId),
}

impl From<VersionSetId> for Condition {
    fn from(value: VersionSetId) -> Self {
        Condition::VersionSetId(value)
    }
}

impl From<StringId> for Condition {
    fn from(value: StringId) -> Self {
        Condition::Extra(value)
    }
}

impl From<Condition> for VersionSetId {
    fn from(value: Condition) -> Self {
        match value {
            Condition::VersionSetId(id) => id,
            Condition::Extra(_) => panic!("Cannot convert Extra to VersionSetId"),
        }
    }
}

/// Specifies a conditional requirement, where the requirement is only active when the condition is met.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ConditionalRequirement {
    /// The conditions that must be met for the requirement to be active.
    pub conditions: Vec<Condition>,
    /// The requirement that is only active when the condition is met.
    pub requirement: Requirement,
}

impl ConditionalRequirement {
    /// Creates a new conditional requirement.
    pub fn new(conditions: Vec<Condition>, requirement: Requirement) -> Self {
        Self {
            conditions,
            requirement,
        }
    }
    /// Returns the version sets that satisfy the requirement.
    pub fn requirement_version_sets<'i>(
        &'i self,
        interner: &'i impl Interner,
    ) -> impl Iterator<Item = VersionSetId> + 'i {
        self.requirement.version_sets(interner)
    }

    /// Returns the version sets that satisfy the requirement, along with the condition that must be met.
    pub fn version_sets_with_condition<'i>(
        &'i self,
        interner: &'i impl Interner,
    ) -> impl Iterator<Item = (VersionSetId, Vec<Condition>)> + 'i {
        self.requirement
            .version_sets(interner)
            .map(move |vs| (vs, self.conditions.clone()))
    }

    /// Returns the condition and requirement.
    pub fn into_condition_and_requirement(self) -> (Vec<Condition>, Requirement) {
        (self.conditions, self.requirement)
    }
}

impl From<Requirement> for ConditionalRequirement {
    fn from(value: Requirement) -> Self {
        Self {
            conditions: vec![],
            requirement: value,
        }
    }
}

impl From<VersionSetId> for ConditionalRequirement {
    fn from(value: VersionSetId) -> Self {
        Self {
            conditions: vec![],
            requirement: value.into(),
        }
    }
}

impl From<VersionSetUnionId> for ConditionalRequirement {
    fn from(value: VersionSetUnionId) -> Self {
        Self {
            conditions: vec![],
            requirement: value.into(),
        }
    }
}

impl From<(VersionSetId, Vec<Condition>)> for ConditionalRequirement {
    fn from((requirement, conditions): (VersionSetId, Vec<Condition>)) -> Self {
        Self {
            conditions,
            requirement: requirement.into(),
        }
    }
}

/// Specifies the dependency of a solvable on a set of version sets.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Requirement {
    /// Specifies a dependency on a single version set.
    Single(VersionSetId),
    /// Specifies a dependency on the union (logical OR) of multiple version sets. A solvable
    /// belonging to _any_ of the version sets contained in the union satisfies the requirement.
    /// This variant is typically used for requirements that can be satisfied by two or more
    /// version sets belonging to _different_ packages.
    Union(VersionSetUnionId),
}

impl Default for Requirement {
    fn default() -> Self {
        Self::Single(Default::default())
    }
}

impl From<VersionSetId> for Requirement {
    fn from(value: VersionSetId) -> Self {
        Requirement::Single(value)
    }
}

impl From<VersionSetUnionId> for Requirement {
    fn from(value: VersionSetUnionId) -> Self {
        Requirement::Union(value)
    }
}

impl Requirement {
    /// Returns an object that implements `Display` for the requirement.
    pub fn display<'i>(&'i self, interner: &'i impl Interner) -> impl Display + '_ {
        DisplayRequirement {
            interner,
            requirement: self,
        }
    }

    pub(crate) fn version_sets<'i>(
        &'i self,
        interner: &'i impl Interner,
    ) -> impl Iterator<Item = VersionSetId> + 'i {
        match *self {
            Requirement::Single(version_set) => {
                itertools::Either::Left(std::iter::once(version_set))
            }
            Requirement::Union(version_set_union) => {
                itertools::Either::Right(interner.version_sets_in_union(version_set_union))
            }
        }
    }
}

pub(crate) struct DisplayRequirement<'i, I: Interner> {
    interner: &'i I,
    requirement: &'i Requirement,
}

impl<'i, I: Interner> Display for DisplayRequirement<'i, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self.requirement {
            Requirement::Single(version_set) => write!(
                f,
                "{} {}",
                self.interner
                    .display_name(self.interner.version_set_name(version_set)),
                self.interner.display_version_set(version_set)
            ),
            Requirement::Union(version_set_union) => {
                let formatted_version_sets = self
                    .interner
                    .version_sets_in_union(version_set_union)
                    .format_with(" | ", |version_set, f| {
                        f(&format_args!(
                            "{} {}",
                            self.interner
                                .display_name(self.interner.version_set_name(version_set)),
                            self.interner.display_version_set(version_set)
                        ))
                    });

                write!(f, "{}", formatted_version_sets)
            }
        }
    }
}
