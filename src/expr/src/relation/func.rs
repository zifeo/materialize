// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

#![allow(missing_docs)]

use std::fmt;
use std::iter;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use dec::OrderedDecimal;
use num::{CheckedAdd, Integer, Signed};
use ordered_float::OrderedFloat;
use regex::Regex;
use serde::{Deserialize, Serialize};

use lowertest::MzEnumReflect;
use ore::cast::CastFrom;
use repr::adt::array::ArrayDimension;
use repr::adt::numeric;
use repr::adt::regex::Regex as ReprRegex;
use repr::{ColumnType, Datum, Diff, RelationType, Row, RowArena, ScalarType};

use crate::relation::{compare_columns, ColumnOrder};
use crate::scalar::func::jsonb_stringify;
use crate::EvalError;

// TODO(jamii) be careful about overflow in sum/avg
// see https://timely.zulipchat.com/#narrow/stream/186635-engineering/topic/additional.20work/near/163507435

fn max_numeric<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<OrderedDecimal<numeric::Numeric>> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_numeric())
        .max();
    x.map(Datum::Numeric).unwrap_or(Datum::Null)
}

fn max_int16<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<i16> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_int16())
        .max();
    Datum::from(x)
}

fn max_int32<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<i32> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_int32())
        .max();
    Datum::from(x)
}

fn max_int64<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<i64> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_int64())
        .max();
    Datum::from(x)
}

fn max_float32<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<OrderedFloat<f32>> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_ordered_float32())
        .max();
    Datum::from(x)
}

fn max_float64<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<OrderedFloat<f64>> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_ordered_float64())
        .max();
    Datum::from(x)
}

fn max_bool<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<bool> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_bool())
        .max();
    Datum::from(x)
}

fn max_string<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    match datums
        .into_iter()
        .filter(|d| !d.is_null())
        .max_by(|a, b| a.unwrap_str().cmp(&b.unwrap_str()))
    {
        Some(datum) => datum,
        None => Datum::Null,
    }
}

fn max_date<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<NaiveDate> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_date())
        .max();
    Datum::from(x)
}

fn max_timestamp<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<NaiveDateTime> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_timestamp())
        .max();
    Datum::from(x)
}

fn max_timestamptz<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<DateTime<Utc>> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_timestamptz())
        .max();
    Datum::from(x)
}

fn min_numeric<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<OrderedDecimal<numeric::Numeric>> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_numeric())
        .min();
    x.map(Datum::Numeric).unwrap_or(Datum::Null)
}

fn min_int16<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<i16> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_int16())
        .min();
    Datum::from(x)
}

fn min_int32<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<i32> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_int32())
        .min();
    Datum::from(x)
}

fn min_int64<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<i64> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_int64())
        .min();
    Datum::from(x)
}

fn min_float32<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<OrderedFloat<f32>> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_ordered_float32())
        .min();
    Datum::from(x)
}

fn min_float64<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<OrderedFloat<f64>> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_ordered_float64())
        .min();
    Datum::from(x)
}

fn min_bool<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<bool> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_bool())
        .min();
    Datum::from(x)
}

fn min_string<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    match datums
        .into_iter()
        .filter(|d| !d.is_null())
        .min_by(|a, b| a.unwrap_str().cmp(&b.unwrap_str()))
    {
        Some(datum) => datum,
        None => Datum::Null,
    }
}

fn min_date<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<NaiveDate> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_date())
        .min();
    Datum::from(x)
}

fn min_timestamp<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<NaiveDateTime> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_timestamp())
        .min();
    Datum::from(x)
}

fn min_timestamptz<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: Option<DateTime<Utc>> = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_timestamptz())
        .min();
    Datum::from(x)
}

fn sum_int16<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let mut datums = datums.into_iter().filter(|d| !d.is_null()).peekable();
    if datums.peek().is_none() {
        Datum::Null
    } else {
        let x: i64 = datums.map(|d| i64::from(d.unwrap_int16())).sum();
        Datum::from(x)
    }
}

fn sum_int32<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let mut datums = datums.into_iter().filter(|d| !d.is_null()).peekable();
    if datums.peek().is_none() {
        Datum::Null
    } else {
        let x: i64 = datums.map(|d| i64::from(d.unwrap_int32())).sum();
        Datum::from(x)
    }
}

fn sum_int64<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let mut datums = datums.into_iter().filter(|d| !d.is_null()).peekable();
    if datums.peek().is_none() {
        Datum::Null
    } else {
        let x: i128 = datums.map(|d| i128::from(d.unwrap_int64())).sum();
        Datum::from(x)
    }
}

fn sum_float32<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let mut datums = datums.into_iter().filter(|d| !d.is_null()).peekable();
    if datums.peek().is_none() {
        Datum::Null
    } else {
        let x: f32 = datums.map(|d| d.unwrap_float32()).sum();
        Datum::from(x)
    }
}

fn sum_float64<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let mut datums = datums.into_iter().filter(|d| !d.is_null()).peekable();
    if datums.peek().is_none() {
        Datum::Null
    } else {
        let x: f64 = datums.map(|d| d.unwrap_float64()).sum();
        Datum::from(x)
    }
}

fn sum_numeric<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let datums = datums
        .into_iter()
        .filter(|d| !d.is_null())
        .map(|d| d.unwrap_numeric().0)
        .collect::<Vec<_>>();
    if datums.is_empty() {
        Datum::Null
    } else {
        let mut cx = numeric::cx_datum();
        let sum = cx.sum(datums.iter());
        Datum::from(sum)
    }
}

fn count<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let x: i64 = datums.into_iter().filter(|d| !d.is_null()).count() as i64;
    Datum::from(x)
}

fn any<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    datums
        .into_iter()
        .fold(Datum::False, |state, next| match (state, next) {
            (Datum::True, _) | (_, Datum::True) => Datum::True,
            (Datum::Null, _) | (_, Datum::Null) => Datum::Null,
            _ => Datum::False,
        })
}

fn all<'a, I>(datums: I) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    datums
        .into_iter()
        .fold(Datum::True, |state, next| match (state, next) {
            (Datum::False, _) | (_, Datum::False) => Datum::False,
            (Datum::Null, _) | (_, Datum::Null) => Datum::Null,
            _ => Datum::True,
        })
}

fn string_agg<'a, I>(datums: I, temp_storage: &'a RowArena, order_by: &[ColumnOrder]) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    const EMPTY_SEP: &'static str = "";

    let datums = order_aggregate_datums(datums, order_by);
    let mut sep_value_pairs = datums.into_iter().filter_map(|d| {
        if d.is_null() {
            return None;
        }
        let mut value_sep = d.unwrap_list().iter();
        match (value_sep.next().unwrap(), value_sep.next().unwrap()) {
            (Datum::Null, _) => None,
            (Datum::String(val), Datum::Null) => Some((EMPTY_SEP, val)),
            (Datum::String(val), Datum::String(sep)) => Some((sep, val)),
            _ => unreachable!(),
        }
    });

    let mut s = String::default();
    match sep_value_pairs.next() {
        // First value not prefixed by its separator
        Some((_, value)) => s.push_str(value),
        // If no non-null values sent, return NULL.
        None => return Datum::Null,
    }

    for (sep, value) in sep_value_pairs {
        s.push_str(sep);
        s.push_str(value);
    }

    Datum::String(temp_storage.push_string(s))
}

fn jsonb_agg<'a, I>(datums: I, temp_storage: &'a RowArena, order_by: &[ColumnOrder]) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let datums = order_aggregate_datums(datums, order_by);
    temp_storage.make_datum(|packer| {
        packer.push_list(datums.into_iter().filter(|d| !d.is_null()));
    })
}

fn jsonb_object_agg<'a, I>(
    datums: I,
    temp_storage: &'a RowArena,
    order_by: &[ColumnOrder],
) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let datums = order_aggregate_datums(datums, order_by);
    temp_storage.make_datum(|packer| {
        let mut datums: Vec<_> = datums
            .into_iter()
            .filter_map(|d| {
                if d.is_null() {
                    return None;
                }
                let mut list = d.unwrap_list().iter();
                let key = list.next().unwrap();
                let val = list.next().unwrap();
                if key.is_null() {
                    // TODO(benesch): this should produce an error, but
                    // aggregate functions cannot presently produce errors.
                    None
                } else {
                    Some((key.unwrap_str(), val))
                }
            })
            .collect();
        // datums are ordered by any ORDER BY clause now, and we want to preserve
        // the last entry for each key, but we also need to present unique and sorted
        // keys to push_dict. Use sort_by here, which is stable, and so will preserve
        // the ORDER BY order. Then reverse and dedup to retain the last of each
        // key. Reverse again so we're back in push_dict order.
        datums.sort_by_key(|(k, _v)| *k);
        datums.reverse();
        datums.dedup_by_key(|(k, _v)| *k);
        datums.reverse();
        packer.push_dict(datums);
    })
}

// Assuming datums is a List, sort them by the 2nd through Nth elements
// corresponding to order_by, then return the 1st element.
fn order_aggregate_datums<'a, I>(
    datums: I,
    order_by: &[ColumnOrder],
) -> impl Iterator<Item = Datum<'a>>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let mut rows: Vec<(Datum, Row)> = datums
        .into_iter()
        .filter_map(|d| {
            let list = d.unwrap_list();
            let expr = list.iter().next().unwrap();
            let order_row = Row::pack(list.iter().skip(1));
            Some((expr, order_row))
        })
        .collect();
    let mut sort_by = |left: &(_, Row), right: &(_, Row)| {
        let left = &left.1;
        let right = &right.1;
        compare_columns(&order_by, &left.unpack(), &right.unpack(), || {
            left.cmp(&right)
        })
    };
    rows.sort_by(&mut sort_by);
    rows.into_iter().map(|(expr, _order_row)| expr)
}

fn array_concat<'a, I>(datums: I, temp_storage: &'a RowArena, order_by: &[ColumnOrder]) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let datums = order_aggregate_datums(datums, order_by);
    let datums: Vec<_> = datums
        .into_iter()
        .map(|d| d.unwrap_array().elements().iter())
        .flatten()
        .collect();
    let dims = ArrayDimension {
        lower_bound: 1,
        length: datums.len(),
    };
    temp_storage.make_datum(|packer| {
        packer.push_array(&[dims], datums).unwrap();
    })
}

fn list_concat<'a, I>(datums: I, temp_storage: &'a RowArena, order_by: &[ColumnOrder]) -> Datum<'a>
where
    I: IntoIterator<Item = Datum<'a>>,
{
    let datums = order_aggregate_datums(datums, order_by);
    temp_storage.make_datum(|packer| {
        packer.push_list(datums.into_iter().map(|d| d.unwrap_list().iter()).flatten());
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash, MzEnumReflect)]
pub enum AggregateFunc {
    MaxNumeric,
    MaxInt16,
    MaxInt32,
    MaxInt64,
    MaxFloat32,
    MaxFloat64,
    MaxBool,
    MaxString,
    MaxDate,
    MaxTimestamp,
    MaxTimestampTz,
    MinNumeric,
    MinInt16,
    MinInt32,
    MinInt64,
    MinFloat32,
    MinFloat64,
    MinBool,
    MinString,
    MinDate,
    MinTimestamp,
    MinTimestampTz,
    SumInt16,
    SumInt32,
    SumInt64,
    SumFloat32,
    SumFloat64,
    SumNumeric,
    Count,
    Any,
    All,
    /// Accumulates `Datum::List`s whose first element is a JSON-typed `Datum`s
    /// into a JSON list. The other elements are columns used by `order_by`.
    ///
    /// WARNING: Unlike the `jsonb_agg` function that is exposed by the SQL
    /// layer, this function filters out `Datum::Null`, for consistency with
    /// the other aggregate functions.
    JsonbAgg {
        order_by: Vec<ColumnOrder>,
    },
    /// Zips `Datum::List`s whose first element is a JSON-typed `Datum`s into a
    /// JSON map. The other elements are columns used by `order_by`.
    ///
    /// WARNING: Unlike the `jsonb_object_agg` function that is exposed by the SQL
    /// layer, this function filters out `Datum::Null`, for consistency with
    /// the other aggregate functions.
    JsonbObjectAgg {
        order_by: Vec<ColumnOrder>,
    },
    /// Accumulates `Datum::List`s whose first element is a `Datum::Array` into a
    /// single `Datum::Array`. The other elements are columns used by `order_by`.
    ArrayConcat {
        order_by: Vec<ColumnOrder>,
    },
    /// Accumulates `Datum::List`s whose first element is a `Datum::List` into a
    /// single `Datum::List`. The other elements are columns used by `order_by`.
    ListConcat {
        order_by: Vec<ColumnOrder>,
    },
    StringAgg {
        order_by: Vec<ColumnOrder>,
    },
    /// Accumulates any number of `Datum::Dummy`s into `Datum::Dummy`.
    ///
    /// Useful for removing an expensive aggregation while maintaining the shape
    /// of a reduce operator.
    Dummy,
}

impl AggregateFunc {
    pub fn eval<'a, I>(&self, datums: I, temp_storage: &'a RowArena) -> Datum<'a>
    where
        I: IntoIterator<Item = Datum<'a>>,
    {
        match self {
            AggregateFunc::MaxNumeric => max_numeric(datums),
            AggregateFunc::MaxInt16 => max_int16(datums),
            AggregateFunc::MaxInt32 => max_int32(datums),
            AggregateFunc::MaxInt64 => max_int64(datums),
            AggregateFunc::MaxFloat32 => max_float32(datums),
            AggregateFunc::MaxFloat64 => max_float64(datums),
            AggregateFunc::MaxBool => max_bool(datums),
            AggregateFunc::MaxString => max_string(datums),
            AggregateFunc::MaxDate => max_date(datums),
            AggregateFunc::MaxTimestamp => max_timestamp(datums),
            AggregateFunc::MaxTimestampTz => max_timestamptz(datums),
            AggregateFunc::MinNumeric => min_numeric(datums),
            AggregateFunc::MinInt16 => min_int16(datums),
            AggregateFunc::MinInt32 => min_int32(datums),
            AggregateFunc::MinInt64 => min_int64(datums),
            AggregateFunc::MinFloat32 => min_float32(datums),
            AggregateFunc::MinFloat64 => min_float64(datums),
            AggregateFunc::MinBool => min_bool(datums),
            AggregateFunc::MinString => min_string(datums),
            AggregateFunc::MinDate => min_date(datums),
            AggregateFunc::MinTimestamp => min_timestamp(datums),
            AggregateFunc::MinTimestampTz => min_timestamptz(datums),
            AggregateFunc::SumInt16 => sum_int16(datums),
            AggregateFunc::SumInt32 => sum_int32(datums),
            AggregateFunc::SumInt64 => sum_int64(datums),
            AggregateFunc::SumFloat32 => sum_float32(datums),
            AggregateFunc::SumFloat64 => sum_float64(datums),
            AggregateFunc::SumNumeric => sum_numeric(datums),
            AggregateFunc::Count => count(datums),
            AggregateFunc::Any => any(datums),
            AggregateFunc::All => all(datums),
            AggregateFunc::JsonbAgg { order_by } => jsonb_agg(datums, temp_storage, order_by),
            AggregateFunc::JsonbObjectAgg { order_by } => {
                jsonb_object_agg(datums, temp_storage, order_by)
            }
            AggregateFunc::ArrayConcat { order_by } => array_concat(datums, temp_storage, order_by),
            AggregateFunc::ListConcat { order_by } => list_concat(datums, temp_storage, order_by),
            AggregateFunc::StringAgg { order_by } => string_agg(datums, temp_storage, order_by),
            AggregateFunc::Dummy => Datum::Dummy,
        }
    }

    /// Returns the output of the aggregation function when applied on an empty
    /// input relation.
    pub fn default(&self) -> Datum<'static> {
        match self {
            AggregateFunc::Count => Datum::Int64(0),
            AggregateFunc::Any => Datum::False,
            AggregateFunc::All => Datum::True,
            AggregateFunc::Dummy => Datum::Dummy,
            _ => Datum::Null,
        }
    }

    /// Returns a datum whose inclusion in the aggregation will not change its
    /// result.
    pub fn identity_datum(&self) -> Datum<'static> {
        match self {
            AggregateFunc::Any => Datum::False,
            AggregateFunc::All => Datum::True,
            AggregateFunc::Dummy => Datum::Dummy,
            AggregateFunc::ArrayConcat { .. } => Datum::empty_array(),
            AggregateFunc::ListConcat { .. } => Datum::empty_list(),
            _ => Datum::Null,
        }
    }

    /// The output column type for the result of an aggregation.
    ///
    /// The output column type also contains nullability information, which
    /// is (without further information) true for aggregations that are not
    /// counts.
    pub fn output_type(&self, input_type: ColumnType) -> ColumnType {
        let scalar_type = match self {
            AggregateFunc::Count => ScalarType::Int64,
            AggregateFunc::Any => ScalarType::Bool,
            AggregateFunc::All => ScalarType::Bool,
            AggregateFunc::JsonbAgg { .. } => ScalarType::Jsonb,
            AggregateFunc::JsonbObjectAgg { .. } => ScalarType::Jsonb,
            AggregateFunc::SumInt16 => ScalarType::Int64,
            AggregateFunc::SumInt32 => ScalarType::Int64,
            AggregateFunc::SumInt64 => ScalarType::Numeric { scale: Some(0) },
            AggregateFunc::ArrayConcat { .. } | AggregateFunc::ListConcat { .. } => {
                match input_type.scalar_type {
                    // The input is wrapped in a Record if there's an ORDER BY, so extract it out.
                    ScalarType::Record { fields, .. } => fields[0].1.scalar_type.clone(),
                    _ => unreachable!(),
                }
            }
            AggregateFunc::StringAgg { .. } => ScalarType::String,
            // Note AggregateFunc::MaxString, MinString rely on returning input
            // type as output type to support the proper return type for
            // character input.
            _ => input_type.scalar_type,
        };
        // Count never produces null, and other aggregations only produce
        // null in the presence of null inputs.
        let nullable = match self {
            AggregateFunc::Count => false,
            _ => input_type.nullable,
        };
        scalar_type.nullable(nullable)
    }

    /// Returns true if the non-null constraint on the aggregation can be
    /// converted into a non-null constraint on its parameter expression, ie.
    /// whether the result of the aggregation is null if all the input values
    /// are null.
    pub fn propagates_nonnull_constraint(&self) -> bool {
        match self {
            AggregateFunc::MaxNumeric
            | AggregateFunc::MaxInt16
            | AggregateFunc::MaxInt32
            | AggregateFunc::MaxInt64
            | AggregateFunc::MaxFloat32
            | AggregateFunc::MaxFloat64
            | AggregateFunc::MaxBool
            | AggregateFunc::MaxString
            | AggregateFunc::MaxDate
            | AggregateFunc::MaxTimestamp
            | AggregateFunc::MaxTimestampTz
            | AggregateFunc::MinNumeric
            | AggregateFunc::MinInt16
            | AggregateFunc::MinInt32
            | AggregateFunc::MinInt64
            | AggregateFunc::MinFloat32
            | AggregateFunc::MinFloat64
            | AggregateFunc::MinBool
            | AggregateFunc::MinString
            | AggregateFunc::MinDate
            | AggregateFunc::MinTimestamp
            | AggregateFunc::MinTimestampTz
            | AggregateFunc::SumInt16
            | AggregateFunc::SumInt32
            | AggregateFunc::SumInt64
            | AggregateFunc::SumFloat32
            | AggregateFunc::SumFloat64
            | AggregateFunc::SumNumeric
            | AggregateFunc::StringAgg { .. } => true,
            // Count is never null
            AggregateFunc::Count => false,
            _ => false,
        }
    }
}

fn jsonb_each<'a>(
    a: Datum<'a>,
    temp_storage: &'a RowArena,
    stringify: bool,
) -> impl Iterator<Item = (Row, Diff)> + 'a {
    // First produce a map, so that a common iterator can be returned.
    let map = match a {
        Datum::Map(dict) => dict,
        _ => repr::DatumMap::empty(),
    };

    map.iter().map(move |(k, mut v)| {
        if stringify {
            v = jsonb_stringify(v, temp_storage);
        }
        (Row::pack_slice(&[Datum::String(k), v]), 1)
    })
}

fn jsonb_object_keys<'a>(a: Datum<'a>) -> impl Iterator<Item = (Row, Diff)> + 'a {
    let map = match a {
        Datum::Map(dict) => dict,
        _ => repr::DatumMap::empty(),
    };

    map.iter()
        .map(move |(k, _)| (Row::pack_slice(&[Datum::String(k)]), 1))
}

fn jsonb_array_elements<'a>(
    a: Datum<'a>,
    temp_storage: &'a RowArena,
    stringify: bool,
) -> impl Iterator<Item = (Row, Diff)> + 'a {
    let list = match a {
        Datum::List(list) => list,
        _ => repr::DatumList::empty(),
    };
    list.iter().map(move |mut e| {
        if stringify {
            e = jsonb_stringify(e, temp_storage);
        }
        (Row::pack_slice(&[e]), 1)
    })
}

fn regexp_extract(a: Datum, r: &AnalyzedRegex) -> Option<(Row, Diff)> {
    let r = r.inner();
    let a = a.unwrap_str();
    let captures = r.captures(a)?;
    let datums = captures
        .iter()
        .skip(1)
        .map(|m| Datum::from(m.map(|m| m.as_str())));
    Some((Row::pack(datums), 1))
}

fn generate_series<N>(
    start: N,
    stop: N,
    step: N,
) -> Result<impl Iterator<Item = (Row, Diff)>, EvalError>
where
    N: Integer + Signed + CheckedAdd + Clone,
    Datum<'static>: From<N>,
{
    if step == N::zero() {
        return Err(EvalError::InvalidParameterValue(
            "step size cannot equal zero".to_owned(),
        ));
    }
    Ok(num::range_step_inclusive(start, stop, step)
        .map(move |i| (Row::pack_slice(&[Datum::from(i)]), 1)))
}

fn unnest_array<'a>(a: Datum<'a>) -> impl Iterator<Item = (Row, Diff)> + 'a {
    a.unwrap_array()
        .elements()
        .iter()
        .map(move |e| (Row::pack_slice(&[e]), 1))
}

fn unnest_list<'a>(a: Datum<'a>) -> impl Iterator<Item = (Row, Diff)> + 'a {
    a.unwrap_list()
        .iter()
        .map(move |e| (Row::pack_slice(&[e]), 1))
}

impl fmt::Display for AggregateFunc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AggregateFunc::MaxNumeric => f.write_str("max"),
            AggregateFunc::MaxInt16 => f.write_str("max"),
            AggregateFunc::MaxInt32 => f.write_str("max"),
            AggregateFunc::MaxInt64 => f.write_str("max"),
            AggregateFunc::MaxFloat32 => f.write_str("max"),
            AggregateFunc::MaxFloat64 => f.write_str("max"),
            AggregateFunc::MaxBool => f.write_str("max"),
            AggregateFunc::MaxString => f.write_str("max"),
            AggregateFunc::MaxDate => f.write_str("max"),
            AggregateFunc::MaxTimestamp => f.write_str("max"),
            AggregateFunc::MaxTimestampTz => f.write_str("max"),
            AggregateFunc::MinNumeric => f.write_str("min"),
            AggregateFunc::MinInt16 => f.write_str("min"),
            AggregateFunc::MinInt32 => f.write_str("min"),
            AggregateFunc::MinInt64 => f.write_str("min"),
            AggregateFunc::MinFloat32 => f.write_str("min"),
            AggregateFunc::MinFloat64 => f.write_str("min"),
            AggregateFunc::MinBool => f.write_str("min"),
            AggregateFunc::MinString => f.write_str("min"),
            AggregateFunc::MinDate => f.write_str("min"),
            AggregateFunc::MinTimestamp => f.write_str("min"),
            AggregateFunc::MinTimestampTz => f.write_str("min"),
            AggregateFunc::SumInt16 => f.write_str("sum"),
            AggregateFunc::SumInt32 => f.write_str("sum"),
            AggregateFunc::SumInt64 => f.write_str("sum"),
            AggregateFunc::SumFloat32 => f.write_str("sum"),
            AggregateFunc::SumFloat64 => f.write_str("sum"),
            AggregateFunc::SumNumeric => f.write_str("sum"),
            AggregateFunc::Count => f.write_str("count"),
            AggregateFunc::Any => f.write_str("any"),
            AggregateFunc::All => f.write_str("all"),
            AggregateFunc::JsonbAgg { .. } => f.write_str("jsonb_agg"),
            AggregateFunc::JsonbObjectAgg { .. } => f.write_str("jsonb_object_agg"),
            AggregateFunc::ArrayConcat { .. } => f.write_str("array_agg"),
            AggregateFunc::ListConcat { .. } => f.write_str("list_agg"),
            AggregateFunc::StringAgg { .. } => f.write_str("string_agg"),
            AggregateFunc::Dummy => f.write_str("dummy"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub struct CaptureGroupDesc {
    pub index: u32,
    pub name: Option<String>,
    pub nullable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub struct AnalyzedRegex(ReprRegex, Vec<CaptureGroupDesc>);

impl AnalyzedRegex {
    pub fn new(s: &str) -> Result<Self, regex::Error> {
        let r = regex::Regex::new(s)?;
        let descs: Vec<_> = r
            .capture_names()
            .enumerate()
            // The first capture is the entire matched string.
            // This will often not be useful, so skip it.
            // If people want it they can just surround their
            // entire regex in an explicit capture group.
            .skip(1)
            .map(|(i, name)| CaptureGroupDesc {
                index: i as u32,
                name: name.map(String::from),
                // TODO -- we can do better.
                // https://github.com/MaterializeInc/materialize/issues/1685
                nullable: true,
            })
            .collect();
        Ok(Self(ReprRegex(r), descs))
    }
    pub fn capture_groups_len(&self) -> usize {
        self.1.len()
    }
    pub fn capture_groups_iter(&self) -> impl Iterator<Item = &CaptureGroupDesc> {
        self.1.iter()
    }
    pub fn inner(&self) -> &Regex {
        &(self.0).0
    }
}

pub fn csv_extract(a: Datum, n_cols: usize) -> impl Iterator<Item = (Row, Diff)> + '_ {
    let bytes = a.unwrap_str().as_bytes();
    let mut row = Row::default();
    let csv_reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(bytes);
    csv_reader.into_records().filter_map(move |res| match res {
        Ok(sr) if sr.len() == n_cols => {
            row.extend(sr.iter().map(|s| Datum::String(s)));
            Some((row.finish_and_reuse(), 1))
        }
        _ => None,
    })
}

pub fn repeat(a: Datum) -> Option<(Row, Diff)> {
    let n = Diff::cast_from(a.unwrap_int64());
    if n != 0 {
        Some((Row::default(), n))
    } else {
        None
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash, MzEnumReflect)]
pub enum TableFunc {
    JsonbEach { stringify: bool },
    JsonbObjectKeys,
    JsonbArrayElements { stringify: bool },
    RegexpExtract(AnalyzedRegex),
    CsvExtract(usize),
    GenerateSeriesInt32,
    GenerateSeriesInt64,
    // TODO(justin): should also possibly have GenerateSeriesTimestamp{,Tz}.
    Repeat,
    UnnestArray { el_typ: ScalarType },
    UnnestList { el_typ: ScalarType },
}

impl TableFunc {
    pub fn eval<'a>(
        &'a self,
        datums: Vec<Datum<'a>>,
        temp_storage: &'a RowArena,
    ) -> Result<Box<dyn Iterator<Item = (Row, Diff)> + 'a>, EvalError> {
        if self.empty_on_null_input() {
            if datums.iter().any(|d| d.is_null()) {
                return Ok(Box::new(vec![].into_iter()));
            }
        }
        match self {
            TableFunc::JsonbEach { stringify } => {
                Ok(Box::new(jsonb_each(datums[0], temp_storage, *stringify)))
            }
            TableFunc::JsonbObjectKeys => Ok(Box::new(jsonb_object_keys(datums[0]))),
            TableFunc::JsonbArrayElements { stringify } => Ok(Box::new(jsonb_array_elements(
                datums[0],
                temp_storage,
                *stringify,
            ))),
            TableFunc::RegexpExtract(a) => Ok(Box::new(regexp_extract(datums[0], a).into_iter())),
            TableFunc::CsvExtract(n_cols) => {
                Ok(Box::new(csv_extract(datums[0], *n_cols).into_iter()))
            }
            TableFunc::GenerateSeriesInt32 => {
                let res = generate_series(
                    datums[0].unwrap_int32(),
                    datums[1].unwrap_int32(),
                    datums[2].unwrap_int32(),
                )?;
                Ok(Box::new(res))
            }
            TableFunc::GenerateSeriesInt64 => {
                let res = generate_series(
                    datums[0].unwrap_int64(),
                    datums[1].unwrap_int64(),
                    datums[2].unwrap_int64(),
                )?;
                Ok(Box::new(res))
            }
            TableFunc::Repeat => Ok(Box::new(repeat(datums[0]).into_iter())),
            TableFunc::UnnestArray { .. } => Ok(Box::new(unnest_array(datums[0]))),
            TableFunc::UnnestList { .. } => Ok(Box::new(unnest_list(datums[0]))),
        }
    }

    pub fn output_type(&self) -> RelationType {
        RelationType::new(match self {
            TableFunc::JsonbEach { stringify: true } => vec![
                ScalarType::String.nullable(false),
                ScalarType::String.nullable(true),
            ],
            TableFunc::JsonbEach { stringify: false } => vec![
                ScalarType::String.nullable(false),
                ScalarType::Jsonb.nullable(false),
            ],
            TableFunc::JsonbObjectKeys => vec![ScalarType::String.nullable(false)],
            TableFunc::JsonbArrayElements { stringify: true } => {
                vec![ScalarType::String.nullable(true)]
            }
            TableFunc::JsonbArrayElements { stringify: false } => {
                vec![ScalarType::Jsonb.nullable(false)]
            }
            TableFunc::RegexpExtract(a) => a
                .capture_groups_iter()
                .map(|cg| ScalarType::String.nullable(cg.nullable))
                .collect(),
            TableFunc::CsvExtract(n_cols) => iter::repeat(ScalarType::String.nullable(false))
                .take(*n_cols)
                .collect(),
            TableFunc::GenerateSeriesInt32 => {
                vec![ScalarType::Int32.nullable(false)]
            }
            TableFunc::GenerateSeriesInt64 => {
                vec![ScalarType::Int64.nullable(false)]
            }
            TableFunc::Repeat => vec![],
            TableFunc::UnnestArray { el_typ } => vec![el_typ.clone().nullable(true)],
            TableFunc::UnnestList { el_typ } => vec![el_typ.clone().nullable(true)],
        })
    }

    pub fn output_arity(&self) -> usize {
        match self {
            TableFunc::JsonbEach { .. } => 2,
            TableFunc::JsonbObjectKeys => 1,
            TableFunc::JsonbArrayElements { .. } => 1,
            TableFunc::RegexpExtract(a) => a.capture_groups_len(),
            TableFunc::CsvExtract(n_cols) => *n_cols,
            TableFunc::GenerateSeriesInt32 => 1,
            TableFunc::GenerateSeriesInt64 => 1,
            TableFunc::Repeat => 0,
            TableFunc::UnnestArray { .. } => 1,
            TableFunc::UnnestList { .. } => 1,
        }
    }

    pub fn empty_on_null_input(&self) -> bool {
        // Warning: this returns currently "true" for all TableFuncs.
        // If adding a TableFunc for which this function will return "false",
        // check the places where `empty_on_null_input` is called to ensure,
        // such as NonNullRequirements that the case this function returns
        // false is properly handled.
        match self {
            TableFunc::JsonbEach { .. }
            | TableFunc::JsonbObjectKeys
            | TableFunc::JsonbArrayElements { .. }
            | TableFunc::GenerateSeriesInt32
            | TableFunc::GenerateSeriesInt64
            | TableFunc::RegexpExtract(_)
            | TableFunc::CsvExtract(_)
            | TableFunc::Repeat
            | TableFunc::UnnestArray { .. }
            | TableFunc::UnnestList { .. } => true,
        }
    }

    /// True iff the table function preserves the append-only property of its input.
    pub fn preserves_monotonicity(&self) -> bool {
        // Most variants preserve monotonicity, but all variants are enumerated to
        // ensure that added variants at least check that this is the case.
        match self {
            TableFunc::JsonbEach { .. } => true,
            TableFunc::JsonbObjectKeys => true,
            TableFunc::JsonbArrayElements { .. } => true,
            TableFunc::RegexpExtract(_) => true,
            TableFunc::CsvExtract(_) => true,
            TableFunc::GenerateSeriesInt32 => true,
            TableFunc::GenerateSeriesInt64 => true,
            TableFunc::Repeat => false,
            TableFunc::UnnestArray { .. } => true,
            TableFunc::UnnestList { .. } => true,
        }
    }
}

impl fmt::Display for TableFunc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TableFunc::JsonbEach { .. } => f.write_str("jsonb_each"),
            TableFunc::JsonbObjectKeys => f.write_str("jsonb_object_keys"),
            TableFunc::JsonbArrayElements { .. } => f.write_str("jsonb_array_elements"),
            TableFunc::RegexpExtract(a) => write!(f, "regexp_extract({:?}, _)", a.0),
            TableFunc::CsvExtract(n_cols) => write!(f, "csv_extract({}, _)", n_cols),
            TableFunc::GenerateSeriesInt32 => f.write_str("generate_series"),
            TableFunc::GenerateSeriesInt64 => f.write_str("generate_series"),
            TableFunc::Repeat => f.write_str("repeat_row"),
            TableFunc::UnnestArray { .. } => f.write_str("unnest_array"),
            TableFunc::UnnestList { .. } => f.write_str("unnest_list"),
        }
    }
}
