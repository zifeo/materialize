// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Install replace certain `Get` operators with their `Let` value.
//!
//! Some `Let` bindings are not useful, for example when they bind
//! a `Get` as their value, or when there is a single corresponding
//! `Get` statement in their body. These cases can be inlined without
//! harming planning.

use crate::TransformArgs;
use expr::{Id, LocalId, MirRelationExpr};

/// Install replace certain `Get` operators with their `Let` value.
#[derive(Debug)]
pub struct InlineLet {
    /// If `true`, inline MFPs around a Get.
    ///
    /// We want this value to be true for the InlineLet call that comes right
    /// before [crate::join_implementation::JoinImplementation] runs because
    /// [crate::join_implementation::JoinImplementation] cannot lift MFPs
    /// through a Let.
    ///
    /// Generally, though, we prefer to be more conservative in our inlining in
    /// order to be able to better detect CSEs.
    pub inline_mfp: bool,
}

impl crate::Transform for InlineLet {
    fn transform(
        &self,
        relation: &mut MirRelationExpr,
        _: TransformArgs,
    ) -> Result<(), crate::TransformError> {
        let mut lets = vec![];
        self.action(relation, &mut lets);
        for (id, value) in lets.into_iter().rev() {
            *relation = MirRelationExpr::Let {
                id,
                value: Box::new(value),
                body: Box::new(relation.take_dangerous()),
            };
        }
        Ok(())
    }
}

impl InlineLet {
    /// Install replace certain `Get` operators with their `Let` value.
    ///
    /// IMPORTANT: This transform is used for cleaning up after `RelationCSE`, which
    /// adds `Let` operators pretty aggressively, leading to very deep dataflows. Nothing
    /// in this transform should lead to expensive recursive traversal of the subgraph,
    /// such as the one in `MirRelationExpr::typ`, since that may result in a stack
    /// overflow.
    pub fn action(
        &self,
        relation: &mut MirRelationExpr,
        lets: &mut Vec<(LocalId, MirRelationExpr)>,
    ) {
        if let MirRelationExpr::Let { id, value, body } = relation {
            self.action(value, lets);

            let mut num_gets = 0;
            body.visit_mut_pre(&mut |relation| match relation {
                MirRelationExpr::Get { id: get_id, .. } if Id::Local(*id) == *get_id => {
                    num_gets += 1;
                }
                _ => (),
            });

            let stripped_value = if self.inline_mfp {
                expr::MapFilterProject::extract_non_errors_from_expr(&**value).1
            } else {
                &**value
            };
            let inlinable = match stripped_value {
                MirRelationExpr::Get { .. } | MirRelationExpr::Constant { .. } => true,
                _ => num_gets <= 1,
            };

            if inlinable {
                // if only used once, just inline it
                body.visit_mut_pre(&mut |relation| match relation {
                    MirRelationExpr::Get { id: get_id, .. } if Id::Local(*id) == *get_id => {
                        *relation = (**value).clone();
                    }
                    _ => (),
                });
            } else {
                // otherwise lift it to the top so it's out of the way
                lets.push((*id, value.take_dangerous()));
            }

            *relation = body.take_dangerous();
            // might be another Let in the body so have to recur here
            self.action(relation, lets);
        } else {
            relation.visit1_mut(|child| self.action(child, lets));
        }
    }
}
