// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Replace operators on constants collections with constant collections.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::iter;

use expr::{AggregateExpr, EvalError, MirRelationExpr, MirScalarExpr, TableFunc};
use repr::{Datum, Diff, RelationType, Row, RowArena};

use crate::{TransformArgs, TransformError};

/// Replace operators on constants collections with constant collections.
#[derive(Debug)]
pub struct FoldConstants {
    /// An optional maximum size, after which optimization can cease.
    ///
    /// The `None` value here indicates no maximum size, but does not
    /// currently guarantee that any constant expression will be reduced
    /// to a `MirRelationExpr::Constant` variant.
    pub limit: Option<usize>,
}

impl crate::Transform for FoldConstants {
    fn transform(
        &self,
        relation: &mut MirRelationExpr,
        _: TransformArgs,
    ) -> Result<(), TransformError> {
        let mut type_stack = Vec::new();
        relation.try_visit_mut(&mut |e| -> Result<(), TransformError> {
            let num_inputs = e.num_inputs();
            let input_types = &type_stack[type_stack.len() - num_inputs..];
            let mut relation_type = e.typ_with_input_types(input_types);
            self.action(e, &mut relation_type, input_types)?;
            type_stack.truncate(type_stack.len() - num_inputs);
            type_stack.push(relation_type);
            Ok(())
        })
    }
}

impl FoldConstants {
    /// Replace operators on constants collections with constant collections.
    ///
    /// This transform will cease optimization if it encounters constant collections
    /// that are larger than `self.limit`, if that is set. It is not guaranteed that
    /// a constant input within the limit will be reduced to a `Constant` variant.
    pub fn action(
        &self,
        relation: &mut MirRelationExpr,
        relation_type: &mut RelationType,
        input_types: &[RelationType],
    ) -> Result<(), TransformError> {
        match relation {
            MirRelationExpr::Constant { .. } => { /* handled after match */ }
            MirRelationExpr::Get { .. } => {}
            MirRelationExpr::Let { .. } => { /* constant prop done in InlineLet */ }
            MirRelationExpr::Reduce {
                input,
                group_key,
                aggregates,
                monotonic: _,
                expected_group_size: _,
            } => {
                let input_typ = input_types.first().unwrap();
                // Reduce expressions to their simplest form.
                for key in group_key.iter_mut() {
                    key.reduce(input_typ);
                }
                for aggregate in aggregates.iter_mut() {
                    aggregate.expr.reduce(input_typ);
                }

                // Guard against evaluating expression that may contain temporal expressions.
                if group_key.iter().any(|e| e.contains_temporal())
                    || aggregates.iter().any(|a| a.expr.contains_temporal())
                {
                    return Ok(());
                }

                if let MirRelationExpr::Constant { rows, .. } = &**input {
                    let new_rows = match rows {
                        Ok(rows) => Self::fold_reduce_constant(group_key, aggregates, rows),
                        Err(e) => Err(e.clone()),
                    };
                    *relation = MirRelationExpr::Constant {
                        rows: new_rows,
                        typ: relation_type.clone(),
                    };
                }
            }
            MirRelationExpr::TopK { .. } => { /*too complicated*/ }
            MirRelationExpr::Negate { input } => {
                if let MirRelationExpr::Constant { rows, .. } = &mut **input {
                    if let Ok(rows) = rows {
                        for (_row, diff) in rows {
                            *diff *= -1;
                        }
                    }
                    *relation = input.take_dangerous();
                }
            }
            MirRelationExpr::DeclareKeys { input, keys: _ } => {
                if let MirRelationExpr::Constant { rows: _, .. } = &mut **input {
                    *relation = input.take_dangerous();
                }
            }
            MirRelationExpr::Threshold { input } => {
                if let MirRelationExpr::Constant { rows, .. } = &mut **input {
                    if let Ok(rows) = rows {
                        rows.retain(|(_, diff)| *diff > 0);
                    }
                    *relation = input.take_dangerous();
                }
            }
            MirRelationExpr::Map { input, scalars } => {
                // Before reducing the scalar expressions, we need to form an appropriate
                // RelationType to provide to each. Each expression needs a different
                // relation type; although we could in principle use `relation_type` here,
                // we shouldn't rely on `reduce` not looking at its cardinality to assess
                // the number of columns.
                let input_arity = input_types.first().unwrap().arity();
                for (index, scalar) in scalars.iter_mut().enumerate() {
                    let mut current_type = repr::RelationType::new(
                        relation_type.column_types[..(input_arity + index)].to_vec(),
                    );
                    for key in relation_type.keys.iter() {
                        if key.iter().all(|i| *i < input_arity + index) {
                            current_type = current_type.with_key(key.clone());
                        }
                    }
                    scalar.reduce(&current_type);
                }

                // Guard against evaluating expression that may contain temporal expressions.
                if scalars.iter().any(|e| e.contains_temporal()) {
                    return Ok(());
                }

                if let MirRelationExpr::Constant { rows, .. } = &**input {
                    let new_rows = match rows {
                        Ok(rows) => rows
                            .iter()
                            .cloned()
                            .map(|(input_row, diff)| {
                                let mut unpacked = input_row.unpack();
                                let temp_storage = RowArena::new();
                                for scalar in scalars.iter() {
                                    unpacked.push(scalar.eval(&unpacked, &temp_storage)?)
                                }
                                Ok::<_, EvalError>((Row::pack_slice(&unpacked), diff))
                            })
                            .collect::<Result<_, _>>(),
                        Err(e) => Err(e.clone()),
                    };
                    *relation = MirRelationExpr::Constant {
                        rows: new_rows,
                        typ: relation_type.clone(),
                    };
                }
            }
            MirRelationExpr::FlatMap {
                input,
                func,
                exprs,
                demand: _,
            } => {
                let input_typ = input_types.first().unwrap();
                for expr in exprs.iter_mut() {
                    expr.reduce(input_typ);
                }

                // Guard against evaluating expression that may contain temporal expressions.
                if exprs.iter().any(|e| e.contains_temporal()) {
                    return Ok(());
                }

                if let MirRelationExpr::Constant { rows, .. } = &**input {
                    let new_rows = match rows {
                        Ok(rows) => Self::fold_flat_map_constant(func, exprs, rows, self.limit),
                        Err(e) => Err(e.clone()),
                    };
                    match new_rows {
                        Ok(None) => {}
                        Ok(Some(rows)) => {
                            *relation = MirRelationExpr::Constant {
                                rows: Ok(rows),
                                typ: relation_type.clone(),
                            };
                        }
                        Err(err) => {
                            *relation = MirRelationExpr::Constant {
                                rows: Err(err),
                                typ: relation_type.clone(),
                            };
                        }
                    };
                }
            }
            MirRelationExpr::Filter { input, predicates } => {
                let input_typ = input_types.first().unwrap();
                for predicate in predicates.iter_mut() {
                    predicate.reduce(input_typ);
                }
                predicates.retain(|p| !p.is_literal_true());

                // Guard against evaluating expression that may contain temporal expressions.
                if predicates.iter().any(|e| e.contains_temporal()) {
                    return Ok(());
                }

                // If any predicate is false, reduce to the empty collection.
                if predicates
                    .iter()
                    .any(|p| p.is_literal_false() || p.is_literal_null())
                {
                    relation.take_safely();
                } else if let MirRelationExpr::Constant { rows, .. } = &**input {
                    // Evaluate errors last, to reduce risk of spurious errors.
                    predicates.sort_by_key(|p| p.is_literal_err());
                    let new_rows = match rows {
                        Ok(rows) => Self::fold_filter_constant(predicates, rows),
                        Err(e) => Err(e.clone()),
                    };
                    *relation = MirRelationExpr::Constant {
                        rows: new_rows,
                        typ: relation_type.clone(),
                    };
                }
            }
            MirRelationExpr::Project { input, outputs } => {
                if let MirRelationExpr::Constant { rows, .. } = &**input {
                    let mut row_packer = Row::default();
                    let new_rows = match rows {
                        Ok(rows) => Ok(rows
                            .iter()
                            .map(|(input_row, diff)| {
                                let datums = input_row.unpack();
                                row_packer.extend(outputs.iter().map(|i| &datums[*i]));
                                (row_packer.finish_and_reuse(), *diff)
                            })
                            .collect()),
                        Err(e) => Err(e.clone()),
                    };
                    *relation = MirRelationExpr::Constant {
                        rows: new_rows,
                        typ: relation_type.clone(),
                    };
                }
            }
            MirRelationExpr::Join {
                inputs,
                equivalences,
                ..
            } => {
                if inputs.iter().any(|e| e.is_empty()) {
                    relation.take_safely();
                } else if let Some(e) = inputs.iter().find_map(|i| match i {
                    MirRelationExpr::Constant { rows: Err(e), .. } => Some(e),
                    _ => None,
                }) {
                    *relation = MirRelationExpr::Constant {
                        rows: Err(e.clone()),
                        typ: relation_type.clone(),
                    };
                } else if inputs
                    .iter()
                    .all(|i| matches!(i, MirRelationExpr::Constant { rows: Ok(_), .. }))
                {
                    // Guard against evaluating expression that may contain temporal expressions.
                    if equivalences
                        .iter()
                        .any(|equiv| equiv.iter().any(|e| e.contains_temporal()))
                    {
                        return Ok(());
                    }

                    // We can fold all constant inputs together, but must apply the constraints to restrict them.
                    // We start with a single 0-ary row.
                    let mut old_rows = vec![(Row::pack::<_, Datum>(None), 1)];
                    let mut row_packer = Row::default();
                    for input in inputs.iter() {
                        if let MirRelationExpr::Constant { rows: Ok(rows), .. } = input {
                            if let Some(limit) = self.limit {
                                if old_rows.len() * rows.len() > limit {
                                    // Bail out if we have produced too many rows.
                                    // TODO: progressively apply equivalences to narrow this count
                                    // as we go, rather than at the end.
                                    return Ok(());
                                }
                            }
                            let mut next_rows = Vec::new();
                            for (old_row, old_count) in old_rows {
                                for (new_row, new_count) in rows.iter() {
                                    row_packer.extend(old_row.iter().chain(new_row.iter()));
                                    next_rows.push((
                                        row_packer.finish_and_reuse(),
                                        old_count * *new_count,
                                    ));
                                }
                            }
                            old_rows = next_rows;
                        }
                    }

                    // Now throw away anything that doesn't satisfy the requisite constraints.
                    old_rows.retain(|(row, _count)| {
                        let datums = row.unpack();
                        let temp_storage = RowArena::new();
                        equivalences.iter().all(|equivalence| {
                            let mut values =
                                equivalence.iter().map(|e| e.eval(&datums, &temp_storage));
                            if let Some(value) = values.next() {
                                values.all(|v| v == value)
                            } else {
                                true
                            }
                        })
                    });

                    *relation = MirRelationExpr::Constant {
                        rows: Ok(old_rows),
                        typ: relation_type.clone(),
                    };
                }
                // TODO: General constant folding for all constant inputs.
            }
            MirRelationExpr::Union { base, inputs } => {
                if let Some(e) = iter::once(&mut **base)
                    .chain(&mut *inputs)
                    .find_map(|i| match i {
                        MirRelationExpr::Constant { rows: Err(e), .. } => Some(e),
                        _ => None,
                    })
                {
                    *relation = MirRelationExpr::Constant {
                        rows: Err(e.clone()),
                        typ: relation_type.clone(),
                    };
                } else {
                    let mut rows = vec![];
                    let mut new_inputs = vec![];

                    for input in iter::once(&mut **base).chain(&mut *inputs) {
                        match input.take_dangerous() {
                            MirRelationExpr::Constant {
                                rows: Ok(rs),
                                typ: _,
                            } => rows.extend(rs),
                            input => new_inputs.push(input),
                        }
                    }
                    if !rows.is_empty() {
                        new_inputs.push(MirRelationExpr::Constant {
                            rows: Ok(rows),
                            typ: relation_type.clone(),
                        });
                    }

                    *relation = MirRelationExpr::union_many(new_inputs, relation_type.clone());
                }
            }
            MirRelationExpr::ArrangeBy { input, .. } => {
                if let MirRelationExpr::Constant { .. } = &**input {
                    *relation = input.take_dangerous();
                }
            }
        }

        // This transformation maintains the invariant that all constant nodes
        // will be consolidated. We have to make a separate check for constant
        // nodes here, since the match arm above might install new constant
        // nodes.
        if let MirRelationExpr::Constant {
            rows: Ok(rows),
            typ,
        } = relation
        {
            // Reduce down to canonical representation.
            let mut accum = HashMap::new();
            for (row, cnt) in rows.drain(..) {
                *accum.entry(row).or_insert(0) += cnt;
            }
            accum.retain(|_k, v| v != &0);
            // `rows` cleared by drain.
            rows.extend(accum.into_iter());
            rows.sort();

            // Re-establish nullability of each column.
            for col_type in typ.column_types.iter_mut() {
                col_type.nullable = false;
            }
            for (row, _) in rows.iter_mut() {
                for (index, datum) in row.iter().enumerate() {
                    if datum.is_null() {
                        typ.column_types[index].nullable = true;
                    }
                }
            }
            *relation_type = typ.clone();
        }

        Ok(())
    }

    fn fold_reduce_constant(
        group_key: &[MirScalarExpr],
        aggregates: &[AggregateExpr],
        rows: &[(Row, Diff)],
    ) -> Result<Vec<(Row, Diff)>, EvalError> {
        // Build a map from `group_key` to `Vec<Vec<an, ..., a1>>)`,
        // where `an` is the input to the nth aggregate function in
        // `aggregates`.
        let mut groups = BTreeMap::new();
        let temp_storage2 = RowArena::new();
        let mut row_packer = Row::default();
        for (row, diff) in rows {
            // We currently maintain the invariant that any negative
            // multiplicities will be consolidated away before they
            // arrive at a reduce.
            assert!(
                *diff > 0,
                "constant folding encountered reduce on collection \
                             with non-positive multiplicities"
            );
            let datums = row.unpack();
            let temp_storage = RowArena::new();
            let key = group_key
                .iter()
                .map(|e| e.eval(&datums, &temp_storage2))
                .collect::<Result<Vec<_>, _>>()?;
            let val = aggregates
                .iter()
                .map(|agg| {
                    row_packer.extend(&[agg.expr.eval(&datums, &temp_storage)?]);
                    Ok::<_, EvalError>(row_packer.finish_and_reuse())
                })
                .collect::<Result<Vec<_>, _>>()?;
            let entry = groups.entry(key).or_insert_with(Vec::new);
            for _ in 0..*diff {
                entry.push(val.clone());
            }
        }

        // For each group, apply the aggregate function to the rows
        // in the group. The output is
        // `Vec<Vec<k1, ..., kn, r1, ..., rn>>`
        // where kn is the nth column of the key and rn is the
        // result of the nth aggregate function for that group.
        let new_rows = groups
            .into_iter()
            .map({
                let mut row_packer = Row::default();
                move |(key, vals)| {
                    let temp_storage = RowArena::new();
                    row_packer.extend(key.into_iter().chain(aggregates.iter().enumerate().map(
                        |(i, agg)| {
                            if agg.distinct {
                                agg.func.eval(
                                    vals.iter()
                                        .map(|val| val[i].unpack_first())
                                        .collect::<HashSet<_>>()
                                        .into_iter(),
                                    &temp_storage,
                                )
                            } else {
                                agg.func.eval(
                                    vals.iter().map(|val| val[i].unpack_first()),
                                    &temp_storage,
                                )
                            }
                        },
                    )));
                    (row_packer.finish_and_reuse(), 1)
                }
            })
            .collect();
        Ok(new_rows)
    }

    fn fold_flat_map_constant(
        func: &TableFunc,
        exprs: &[MirScalarExpr],
        rows: &[(Row, Diff)],
        limit: Option<usize>,
    ) -> Result<Option<Vec<(Row, Diff)>>, EvalError> {
        // We cannot exceed `usize::MAX` in any array, so this is a fine upper bound.
        let limit = limit.unwrap_or(usize::MAX);
        let mut new_rows = Vec::new();
        let mut row_packer = Row::default();
        for (input_row, diff) in rows {
            let datums = input_row.unpack();
            let temp_storage = RowArena::new();
            let mut output_rows = func
                .eval(
                    exprs
                        .iter()
                        .map(|expr| expr.eval(&datums, &temp_storage))
                        .collect::<Result<Vec<_>, _>>()?,
                    &temp_storage,
                )?
                .fuse();
            for (output_row, diff2) in (&mut output_rows).take(limit - new_rows.len()) {
                row_packer.extend(input_row.clone().into_iter().chain(output_row.into_iter()));
                new_rows.push((row_packer.finish_and_reuse(), diff2 * *diff))
            }
            // If we still have records to enumerate, but dropped out of the iteration,
            // it means we have exhausted `limit` and should stop.
            if output_rows.next() != None {
                return Ok(None);
            }
        }
        Ok(Some(new_rows))
    }

    fn fold_filter_constant(
        predicates: &[MirScalarExpr],
        rows: &[(Row, Diff)],
    ) -> Result<Vec<(Row, Diff)>, EvalError> {
        let mut new_rows = Vec::new();
        'outer: for (row, diff) in rows {
            let datums = row.unpack();
            let temp_storage = RowArena::new();
            for p in &*predicates {
                if p.eval(&datums, &temp_storage)? != Datum::True {
                    continue 'outer;
                }
            }
            new_rows.push((row.clone(), *diff))
        }
        Ok(new_rows)
    }
}
