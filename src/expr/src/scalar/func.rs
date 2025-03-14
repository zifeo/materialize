// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::borrow::Cow;
use std::cmp::{self, Ordering};
use std::convert::{TryFrom, TryInto};
use std::fmt;
use std::iter;
use std::str;

use ::encoding::label::encoding_from_whatwg_label;
use ::encoding::DecoderTrap;
use chrono::{
    DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Timelike,
    Utc,
};
use dec::Rounding;
use hmac::{Hmac, Mac, NewMac};
use itertools::Itertools;
use md5::{Digest, Md5};
use ordered_float::OrderedFloat;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Sha224, Sha256, Sha384, Sha512};

use lowertest::MzEnumReflect;
use ore::collections::CollectionExt;
use ore::fmt::FormatBuffer;
use ore::result::ResultExt;
use ore::str::StrExt;
use pgrepr::Type;
use repr::adt::array::ArrayDimension;
use repr::adt::datetime::{DateTimeUnits, Timezone};
use repr::adt::interval::Interval;
use repr::adt::jsonb::JsonbRef;
use repr::adt::numeric::{self, Numeric};
use repr::adt::regex::Regex;
use repr::{strconv, ColumnName, ColumnType, Datum, Row, RowArena, ScalarType};

use crate::scalar::func::format::DateTimeFormat;
use crate::{like_pattern, EvalError, MirScalarExpr};

#[macro_use]
mod macros;
mod encoding;
mod format;
mod impls;

pub use impls::*;

#[derive(
    Ord, PartialOrd, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash, MzEnumReflect,
)]
pub enum NullaryFunc {
    MzLogicalTimestamp,
}

impl NullaryFunc {
    pub fn output_type(&self) -> ColumnType {
        match self {
            NullaryFunc::MzLogicalTimestamp => {
                ScalarType::Numeric { scale: Some(0) }.nullable(false)
            }
        }
    }
}

impl fmt::Display for NullaryFunc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            NullaryFunc::MzLogicalTimestamp => f.write_str("mz_logical_timestamp"),
        }
    }
}

pub fn and<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
    a_expr: &'a MirScalarExpr,
    b_expr: &'a MirScalarExpr,
) -> Result<Datum<'a>, EvalError> {
    match a_expr.eval(datums, temp_storage)? {
        Datum::False => Ok(Datum::False),
        a => match (a, b_expr.eval(datums, temp_storage)?) {
            (_, Datum::False) => Ok(Datum::False),
            (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
            (Datum::True, Datum::True) => Ok(Datum::True),
            _ => unreachable!(),
        },
    }
}

pub fn or<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
    a_expr: &'a MirScalarExpr,
    b_expr: &'a MirScalarExpr,
) -> Result<Datum<'a>, EvalError> {
    match a_expr.eval(datums, temp_storage)? {
        Datum::True => Ok(Datum::True),
        a => match (a, b_expr.eval(datums, temp_storage)?) {
            (_, Datum::True) => Ok(Datum::True),
            (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
            (Datum::False, Datum::False) => Ok(Datum::False),
            _ => unreachable!(),
        },
    }
}

fn abs_numeric<'a>(a: Datum<'a>) -> Datum<'a> {
    let mut a = a.unwrap_numeric();
    numeric::cx_datum().abs(&mut a.0);
    Datum::Numeric(a)
}

fn cast_bool_to_string<'a>(a: Datum<'a>) -> Datum<'a> {
    match a.unwrap_bool() {
        true => Datum::from("true"),
        false => Datum::from("false"),
    }
}

fn cast_bool_to_string_nonstandard<'a>(a: Datum<'a>) -> Datum<'a> {
    // N.B. this function differs from `cast_bool_to_string_implicit` because
    // the SQL specification requires `true` and `false` to be spelled out in
    // explicit casts, while PostgreSQL prefers its more concise `t` and `f`
    // representation in some contexts, for historical reasons.
    Datum::String(strconv::format_bool_static(a.unwrap_bool()))
}

fn cast_bool_to_int32<'a>(a: Datum<'a>) -> Datum<'a> {
    match a.unwrap_bool() {
        true => Datum::Int32(1),
        false => Datum::Int32(0),
    }
}
fn cast_int16_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_int16(&mut buf, a.unwrap_int16());
    Datum::String(temp_storage.push_string(buf))
}

fn cast_int16_to_float32<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int16() as f32)
}

fn cast_int16_to_float64<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(f64::from(a.unwrap_int16()))
}

fn cast_int16_to_int32<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(i32::from(a.unwrap_int16()))
}

fn cast_int16_to_int64<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(i64::from(a.unwrap_int16()))
}

fn cast_int16_to_numeric<'a>(a: Datum<'a>, scale: Option<u8>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_int16();
    let mut a = Numeric::from(i32::from(a));
    if let Some(scale) = scale {
        if numeric::rescale(&mut a, scale).is_err() {
            return Err(EvalError::NumericFieldOverflow);
        }
    }
    // Besides `rescale`, cast is infallible.
    Ok(Datum::from(a))
}

fn cast_int32_to_bool<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int32() != 0)
}

fn cast_int32_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_int32(&mut buf, a.unwrap_int32());
    Datum::String(temp_storage.push_string(buf))
}

fn cast_int32_to_float32<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int32() as f32)
}

fn cast_int32_to_float64<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(f64::from(a.unwrap_int32()))
}

fn cast_int32_to_int16<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    match i16::try_from(a.unwrap_int32()) {
        Ok(n) => Ok(Datum::from(n)),
        Err(_) => Err(EvalError::Int16OutOfRange),
    }
}

fn cast_int32_to_int64<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(i64::from(a.unwrap_int32()))
}

fn cast_int32_to_numeric<'a>(a: Datum<'a>, scale: Option<u8>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_int32();
    let mut a = Numeric::from(a);
    if let Some(scale) = scale {
        if numeric::rescale(&mut a, scale).is_err() {
            return Err(EvalError::NumericFieldOverflow);
        }
    }
    // Besides `rescale`, cast is infallible.
    Ok(Datum::from(a))
}

fn cast_int64_to_bool<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int64() != 0)
}

fn cast_int64_to_int16<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    match i16::try_from(a.unwrap_int64()) {
        Ok(n) => Ok(Datum::from(n)),
        Err(_) => Err(EvalError::Int16OutOfRange),
    }
}

fn cast_int64_to_int32<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    match i32::try_from(a.unwrap_int64()) {
        Ok(n) => Ok(Datum::from(n)),
        Err(_) => Err(EvalError::Int32OutOfRange),
    }
}

fn cast_int64_to_numeric<'a>(a: Datum<'a>, scale: Option<u8>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_int64();
    let mut a = Numeric::from(a);
    if let Some(scale) = scale {
        if numeric::rescale(&mut a, scale).is_err() {
            return Err(EvalError::NumericFieldOverflow);
        }
    }
    // Besides `rescale`, cast is infallible.
    Ok(Datum::from(a))
}

fn cast_int64_to_float32<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int64() as f32)
}

fn cast_int64_to_float64<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int64() as f64)
}

fn cast_int64_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_int64(&mut buf, a.unwrap_int64());
    Datum::String(temp_storage.push_string(buf))
}

fn cast_float32_to_numeric<'a>(a: Datum<'a>, scale: Option<u8>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float32();
    if a.is_infinite() {
        return Err(EvalError::InfinityOutOfDomain(
            "casting real to numeric".to_owned(),
        ));
    }
    let mut a = Numeric::from(a);
    if let Some(scale) = scale {
        if numeric::rescale(&mut a, scale).is_err() {
            return Err(EvalError::NumericFieldOverflow);
        }
    }
    numeric::munge_numeric(&mut a).unwrap();
    Ok(Datum::from(a))
}

fn cast_float64_to_numeric<'a>(a: Datum<'a>, scale: Option<u8>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float64();
    if a.is_infinite() {
        return Err(EvalError::InfinityOutOfDomain(
            "casting double precision to numeric".to_owned(),
        ));
    }
    let mut a = Numeric::from(a);
    if let Some(scale) = scale {
        if numeric::rescale(&mut a, scale).is_err() {
            return Err(EvalError::NumericFieldOverflow);
        }
    }
    match numeric::munge_numeric(&mut a) {
        Ok(_) => Ok(Datum::from(a)),
        Err(_) => Err(EvalError::NumericFieldOverflow),
    }
}

fn cast_numeric_to_int16<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric().0;
    let mut cx = numeric::cx_datum();
    cx.round(&mut a);
    cx.clear_status();
    let i = cx.try_into_i32(a).map_err(|_| EvalError::Int16OutOfRange)?;
    match i16::try_from(i) {
        Ok(i) => Ok(Datum::from(i)),
        Err(_) => Err(EvalError::Int16OutOfRange),
    }
}

fn cast_numeric_to_int32<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric().0;
    let mut cx = numeric::cx_datum();
    cx.round(&mut a);
    cx.clear_status();
    match cx.try_into_i32(a) {
        Ok(i) => Ok(Datum::from(i)),
        Err(_) => Err(EvalError::Int32OutOfRange),
    }
}

fn cast_numeric_to_int64<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric().0;
    let mut cx = numeric::cx_datum();
    cx.round(&mut a);
    cx.clear_status();
    match cx.try_into_i64(a) {
        Ok(i) => Ok(Datum::from(i)),
        Err(_) => Err(EvalError::Int64OutOfRange),
    }
}

fn cast_numeric_to_float32<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_numeric().0;
    let i = a.to_string().parse::<f32>().unwrap();
    if i.is_infinite() {
        Err(EvalError::Float32OutOfRange)
    } else {
        Ok(Datum::from(i))
    }
}

fn cast_numeric_to_float64<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_numeric().0;
    let i = a.to_string().parse::<f64>().unwrap();
    if i.is_infinite() {
        Err(EvalError::Float64OutOfRange)
    } else {
        Ok(Datum::from(i))
    }
}

fn cast_numeric_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_numeric(&mut buf, &a.unwrap_numeric());
    Datum::String(temp_storage.push_string(buf))
}

fn cast_string_to_bool<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    match strconv::parse_bool(a.unwrap_str())? {
        true => Ok(Datum::True),
        false => Ok(Datum::False),
    }
}

fn cast_string_to_bytes<'a>(
    a: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let bytes = strconv::parse_bytes(a.unwrap_str())?;
    Ok(Datum::Bytes(temp_storage.push_bytes(bytes)))
}

fn cast_string_to_int16<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    strconv::parse_int16(a.unwrap_str())
        .map(Datum::Int16)
        .err_into()
}

fn cast_string_to_int32<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    strconv::parse_int32(a.unwrap_str())
        .map(Datum::Int32)
        .err_into()
}

fn cast_string_to_int64<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    strconv::parse_int64(a.unwrap_str())
        .map(Datum::Int64)
        .err_into()
}

fn cast_string_to_float32<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    strconv::parse_float32(a.unwrap_str())
        .map(|n| Datum::Float32(n.into()))
        .err_into()
}

fn cast_string_to_float64<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    strconv::parse_float64(a.unwrap_str())
        .map(|n| Datum::Float64(n.into()))
        .err_into()
}

fn cast_string_to_numeric<'a>(a: Datum<'a>, scale: Option<u8>) -> Result<Datum<'a>, EvalError> {
    let mut d = strconv::parse_numeric(a.unwrap_str())?;
    if let Some(scale) = scale {
        if numeric::rescale(&mut d.0, scale).is_err() {
            return Err(EvalError::NumericFieldOverflow);
        }
    }
    Ok(Datum::Numeric(d))
}

fn cast_string_to_date<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    strconv::parse_date(a.unwrap_str())
        .map(Datum::Date)
        .err_into()
}

fn cast_string_to_array<'a>(
    a: Datum<'a>,
    cast_expr: &'a MirScalarExpr,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let datums = strconv::parse_array(
        a.unwrap_str(),
        || Datum::Null,
        |elem_text| {
            let elem_text = match elem_text {
                Cow::Owned(s) => temp_storage.push_string(s),
                Cow::Borrowed(s) => s,
            };
            cast_expr.eval(&[Datum::String(elem_text)], temp_storage)
        },
    )?;
    array_create_scalar(&datums, temp_storage)
}

fn cast_string_to_list<'a>(
    a: Datum<'a>,
    list_typ: &ScalarType,
    cast_expr: &'a MirScalarExpr,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let parsed_datums = strconv::parse_list(
        a.unwrap_str(),
        matches!(list_typ.unwrap_list_element_type(), ScalarType::List { .. }),
        || Datum::Null,
        |elem_text| {
            let elem_text = match elem_text {
                Cow::Owned(s) => temp_storage.push_string(s),
                Cow::Borrowed(s) => s,
            };
            cast_expr.eval(&[Datum::String(elem_text)], temp_storage)
        },
    )?;

    Ok(temp_storage.make_datum(|packer| packer.push_list(parsed_datums)))
}

fn cast_string_to_map<'a>(
    a: Datum<'a>,
    map_typ: &ScalarType,
    cast_expr: &'a MirScalarExpr,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let parsed_map = strconv::parse_map(
        a.unwrap_str(),
        matches!(map_typ.unwrap_map_value_type(), ScalarType::Map { .. }),
        |value_text| -> Result<Datum, EvalError> {
            let value_text = match value_text {
                Cow::Owned(s) => temp_storage.push_string(s),
                Cow::Borrowed(s) => s,
            };
            cast_expr.eval(&[Datum::String(value_text)], temp_storage)
        },
    )?;
    let mut pairs: Vec<(String, Datum)> = parsed_map.into_iter().map(|(k, v)| (k, v)).collect();
    pairs.sort_by(|(k1, _v1), (k2, _v2)| k1.cmp(k2));
    pairs.dedup_by(|(k1, _v1), (k2, _v2)| k1 == k2);
    Ok(temp_storage.make_datum(|packer| {
        packer.push_dict_with(|packer| {
            for (k, v) in pairs {
                packer.push(Datum::String(&k));
                packer.push(v);
            }
        })
    }))
}

fn cast_string_to_time<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    strconv::parse_time(a.unwrap_str())
        .map(Datum::Time)
        .err_into()
}

fn cast_string_to_timestamp<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    strconv::parse_timestamp(a.unwrap_str())
        .map(Datum::Timestamp)
        .err_into()
}

fn cast_string_to_timestamptz<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    strconv::parse_timestamptz(a.unwrap_str())
        .map(Datum::TimestampTz)
        .err_into()
}

fn cast_string_to_interval<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    strconv::parse_interval(a.unwrap_str())
        .map(Datum::Interval)
        .err_into()
}

fn cast_string_to_uuid<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    strconv::parse_uuid(a.unwrap_str())
        .map(Datum::Uuid)
        .err_into()
}

fn cast_str_to_char<'a>(
    a: Datum<'a>,
    length: Option<usize>,
    fail_on_len: bool,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let s =
        repr::adt::char::format_str_trim(a.unwrap_str(), length, fail_on_len).map_err(|_| {
            assert!(fail_on_len);
            EvalError::StringValueTooLong {
                target_type: "character".to_string(),
                length: length.unwrap(),
            }
        })?;

    Ok(Datum::String(temp_storage.push_string(s)))
}

fn pad_char<'a>(
    a: Datum<'a>,
    length: Option<usize>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let s = repr::adt::char::format_str_pad(a.unwrap_str(), length);
    Ok(Datum::String(temp_storage.push_string(s)))
}

fn cast_string_to_varchar<'a>(
    a: Datum<'a>,
    length: Option<usize>,
    fail_on_len: bool,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let s = repr::adt::varchar::format_str(a.unwrap_str(), length, fail_on_len).map_err(|_| {
        assert!(fail_on_len);
        EvalError::StringValueTooLong {
            target_type: "character varying".to_string(),
            length: length.unwrap(),
        }
    })?;
    Ok(Datum::String(temp_storage.push_string(s)))
}

fn cast_date_to_timestamp<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::Timestamp(a.unwrap_date().and_hms(0, 0, 0))
}

fn cast_date_to_timestamptz<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::TimestampTz(DateTime::<Utc>::from_utc(
        a.unwrap_date().and_hms(0, 0, 0),
        Utc,
    ))
}

fn cast_date_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_date(&mut buf, a.unwrap_date());
    Datum::String(temp_storage.push_string(buf))
}

fn cast_time_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_time(&mut buf, a.unwrap_time());
    Datum::String(temp_storage.push_string(buf))
}

fn cast_time_to_interval<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let t = a.unwrap_time();
    match Interval::new(
        0,
        t.num_seconds_from_midnight() as i64,
        t.nanosecond() as i64,
    ) {
        Ok(i) => Ok(Datum::Interval(i)),
        Err(_) => Err(EvalError::IntervalOutOfRange),
    }
}

fn cast_timestamp_to_date<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::Date(a.unwrap_timestamp().date())
}

fn cast_timestamp_to_timestamptz<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::TimestampTz(DateTime::<Utc>::from_utc(a.unwrap_timestamp(), Utc))
}

fn cast_timestamp_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_timestamp(&mut buf, a.unwrap_timestamp());
    Datum::String(temp_storage.push_string(buf))
}

fn cast_timestamptz_to_date<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::Date(a.unwrap_timestamptz().naive_utc().date())
}

fn cast_timestamptz_to_timestamp<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::Timestamp(a.unwrap_timestamptz().naive_utc())
}

fn cast_timestamptz_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_timestamptz(&mut buf, a.unwrap_timestamptz());
    Datum::String(temp_storage.push_string(buf))
}

fn cast_interval_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_interval(&mut buf, a.unwrap_interval());
    Datum::String(temp_storage.push_string(buf))
}

fn cast_interval_to_time<'a>(a: Datum<'a>) -> Datum<'a> {
    let mut i = a.unwrap_interval();

    // Negative durations have their HH::MM::SS.NS values subtracted from 1 day.
    if i.duration < 0 {
        i = Interval::new(0, 86400, 0)
            .unwrap()
            .checked_add(
                &Interval::new(0, i.dur_as_secs() % (24 * 60 * 60), i.nanoseconds() as i64)
                    .unwrap(),
            )
            .unwrap();
    }

    Datum::Time(NaiveTime::from_hms_nano(
        i.hours() as u32,
        i.minutes() as u32,
        i.seconds() as u32,
        i.nanoseconds() as u32,
    ))
}

fn cast_bytes_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_bytes(&mut buf, a.unwrap_bytes());
    Datum::String(temp_storage.push_string(buf))
}

// TODO(jamii): it would be much more efficient to skip the intermediate
// repr::jsonb::Jsonb.
fn cast_string_to_jsonb<'a>(
    a: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let jsonb = strconv::parse_jsonb(a.unwrap_str())?;
    Ok(temp_storage.push_unary_row(jsonb.into_row()))
}

fn cast_jsonb_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_jsonb(&mut buf, JsonbRef::from_datum(a));
    Datum::String(temp_storage.push_string(buf))
}

pub fn jsonb_stringify<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    match a {
        Datum::JsonNull => Datum::Null,
        Datum::String(_) => a,
        _ => cast_jsonb_to_string(a, temp_storage),
    }
}

fn cast_jsonb_or_null_to_jsonb<'a>(a: Datum<'a>) -> Datum<'a> {
    match a {
        Datum::Null => Datum::JsonNull,
        Datum::Float64(f) => {
            if f.is_finite() {
                a
            } else if f.is_nan() {
                Datum::String("NaN")
            } else if f.is_sign_positive() {
                Datum::String("Infinity")
            } else {
                Datum::String("-Infinity")
            }
        }
        _ => a,
    }
}

fn cast_jsonb_to_int16<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    match a {
        Datum::Int64(_) => cast_int64_to_int16(a),
        Datum::Float64(_) => cast_int64_to_int16(a),
        _ => Err(EvalError::InvalidJsonbCast {
            from: jsonb_type(a).into(),
            to: "smallint".into(),
        }),
    }
}

fn cast_jsonb_to_int32<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    match a {
        Datum::Int64(_) => cast_int64_to_int32(a),
        Datum::Float64(f) => cast_float64_to_int32(Some(*f)).map(|f| f.into()),
        _ => Err(EvalError::InvalidJsonbCast {
            from: jsonb_type(a).into(),
            to: "integer".into(),
        }),
    }
}

fn cast_jsonb_to_int64<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    match a {
        Datum::Int64(_) => Ok(a),
        Datum::Float64(f) => cast_float64_to_int64(Some(*f)).map(|f| f.into()),
        _ => Err(EvalError::InvalidJsonbCast {
            from: jsonb_type(a).into(),
            to: "bigint".into(),
        }),
    }
}

fn cast_jsonb_to_float32<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    match a {
        Datum::Int64(_) => Ok(cast_int64_to_float32(a)),
        Datum::Float64(f) => cast_float64_to_float32(Some(*f)).map(|f| f.into()),
        _ => Err(EvalError::InvalidJsonbCast {
            from: jsonb_type(a).into(),
            to: "real".into(),
        }),
    }
}

fn cast_jsonb_to_float64<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    match a {
        Datum::Int64(_) => Ok(cast_int64_to_float64(a)),
        Datum::Float64(_) => Ok(a),
        _ => Err(EvalError::InvalidJsonbCast {
            from: jsonb_type(a).into(),
            to: "double precision".into(),
        }),
    }
}

fn cast_jsonb_to_numeric<'a>(a: Datum<'a>, scale: Option<u8>) -> Result<Datum<'a>, EvalError> {
    match a {
        Datum::Int64(_) => cast_int64_to_numeric(a, scale),
        Datum::Float64(_) => cast_float64_to_numeric(a, scale),
        _ => Err(EvalError::InvalidJsonbCast {
            from: jsonb_type(a).into(),
            to: "numeric".into(),
        }),
    }
}

fn cast_jsonb_to_bool<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    match a {
        Datum::True | Datum::False => Ok(a),
        _ => Err(EvalError::InvalidJsonbCast {
            from: jsonb_type(a).into(),
            to: "boolean".into(),
        }),
    }
}

fn jsonb_type(d: Datum<'_>) -> &'static str {
    match d {
        Datum::JsonNull => "null",
        Datum::False | Datum::True => "boolean",
        Datum::String(_) => "string",
        Datum::Int64(_) | Datum::Float64(_) => "numeric",
        Datum::List(_) => "array",
        Datum::Map(_) => "object",
        _ => unreachable!("jsonb_type called on invalid datum {:?}", d),
    }
}

fn cast_uuid_to_string<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_uuid(&mut buf, a.unwrap_uuid());
    Datum::String(temp_storage.push_string(buf))
}

/// Casts between two list types by casting each element of `a` ("list1") using
/// `cast_expr` and collecting the results into a new list ("list2").
fn cast_list1_to_list2<'a>(
    a: Datum,
    cast_expr: &'a MirScalarExpr,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let mut cast_datums = Vec::new();
    for el in a.unwrap_list().iter() {
        // `cast_expr` is evaluated as an expression that casts the
        // first column in `datums` (i.e. `datums[0]`) from the list elements'
        // current type to a target type.
        cast_datums.push(cast_expr.eval(&[el], temp_storage)?);
    }

    Ok(temp_storage.make_datum(|packer| packer.push_list(cast_datums)))
}

fn add_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int16()
        .checked_add(b.unwrap_int16())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

fn add_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int32()
        .checked_add(b.unwrap_int32())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

fn add_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int64()
        .checked_add(b.unwrap_int64())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

fn add_float32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_float32() + b.unwrap_float32())
}

fn add_float64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_float64() + b.unwrap_float64())
}

fn add_timestamp_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let dt = a.unwrap_timestamp();
    Datum::Timestamp(match b {
        Datum::Interval(i) => {
            let dt = add_timestamp_months(dt, i.months);
            dt + i.duration_as_chrono()
        }
        _ => panic!("Tried to do timestamp addition with non-interval: {:?}", b),
    })
}

fn add_timestamptz_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let dt = a.unwrap_timestamptz().naive_utc();

    let new_ndt = match b {
        Datum::Interval(i) => {
            let dt = add_timestamp_months(dt, i.months);
            dt + i.duration_as_chrono()
        }
        _ => panic!("Tried to do timestamp addition with non-interval: {:?}", b),
    };

    Datum::TimestampTz(DateTime::<Utc>::from_utc(new_ndt, Utc))
}

fn add_date_time<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let date = a.unwrap_date();
    let time = b.unwrap_time();

    Datum::Timestamp(
        NaiveDate::from_ymd(date.year(), date.month(), date.day()).and_hms_nano(
            time.hour(),
            time.minute(),
            time.second(),
            time.nanosecond(),
        ),
    )
}

fn add_date_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let date = a.unwrap_date();
    let interval = b.unwrap_interval();

    let dt = NaiveDate::from_ymd(date.year(), date.month(), date.day()).and_hms(0, 0, 0);
    let dt = add_timestamp_months(dt, interval.months);
    Datum::Timestamp(dt + interval.duration_as_chrono())
}

fn add_time_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let time = a.unwrap_time();
    let interval = b.unwrap_interval();
    let (t, _) = time.overflowing_add_signed(interval.duration_as_chrono());
    Datum::Time(t)
}

fn ceil_numeric<'a>(a: Datum<'a>) -> Datum<'a> {
    let mut d = a.unwrap_numeric();
    // ceil will be nop if has no fractional digits.
    if d.0.exponent() >= 0 {
        return a;
    }
    let mut cx = numeric::cx_datum();
    cx.set_rounding(Rounding::Ceiling);
    cx.round(&mut d.0);
    numeric::munge_numeric(&mut d.0).unwrap();
    Datum::Numeric(d)
}

fn floor_numeric<'a>(a: Datum<'a>) -> Datum<'a> {
    let mut d = a.unwrap_numeric();
    // floor will be nop if has no fractional digits.
    if d.0.exponent() >= 0 {
        return a;
    }
    let mut cx = numeric::cx_datum();
    cx.set_rounding(Rounding::Floor);
    cx.round(&mut d.0);
    numeric::munge_numeric(&mut d.0).unwrap();
    Datum::Numeric(d)
}

fn round_numeric_unary<'a>(a: Datum<'a>) -> Datum<'a> {
    let mut d = a.unwrap_numeric();
    // round will be nop if has no fractional digits.
    if d.0.exponent() >= 0 {
        return a;
    }
    numeric::cx_datum().round(&mut d.0);
    Datum::Numeric(d)
}

fn round_numeric_binary<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric().0;
    let mut b = b.unwrap_int32();
    let mut cx = numeric::cx_datum();
    let a_exp = a.exponent();
    if a_exp > 0 && b > 0 || a_exp < 0 && -a_exp < b {
        // This condition indicates:
        // - a is a value without a decimal point, b is a positive number
        // - a has a decimal point, but b is larger than its scale
        // In both of these situations, right-pad the number with zeroes, which // is most easily done with rescale.

        // Ensure rescale doesn't exceed max precision by putting a ceiling on
        // b equal to the maximum remaining scale the value can support.
        b = std::cmp::min(
            b,
            (numeric::NUMERIC_DATUM_MAX_PRECISION as u32
                - (numeric::get_precision(&a) - u32::from(numeric::get_scale(&a))))
                as i32,
        );
        cx.rescale(&mut a, &numeric::Numeric::from(-b));
    } else {
        // To avoid invalid operations, clamp b to be within 1 more than the
        // precision limit.
        const MAX_P_LIMIT: i32 = 1 + numeric::NUMERIC_DATUM_MAX_PRECISION as i32;
        b = std::cmp::min(MAX_P_LIMIT, b);
        b = std::cmp::max(-MAX_P_LIMIT, b);
        let mut b = numeric::Numeric::from(b);
        // Shift by 10^b; this put digit to round to in the one's place.
        cx.scaleb(&mut a, &b);
        cx.round(&mut a);
        // Negate exponent for shift back
        cx.neg(&mut b);
        cx.scaleb(&mut a, &b);
    }

    if cx.status().overflow() {
        Err(EvalError::FloatOverflow)
    } else if a.is_zero() {
        // simpler than handling cases where exponent has gotten set to some
        // value greater than the max precision, but all significant digits
        // were rounded away.
        Ok(Datum::from(numeric::Numeric::zero()))
    } else {
        numeric::munge_numeric(&mut a).unwrap();
        Ok(Datum::from(a))
    }
}

fn convert_from<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    // Convert PostgreSQL-style encoding names[1] to WHATWG-style encoding names[2],
    // which the encoding library uses[3].
    // [1]: https://www.postgresql.org/docs/9.5/multibyte.html
    // [2]: https://encoding.spec.whatwg.org/
    // [3]: https://github.com/lifthrasiir/rust-encoding/blob/4e79c35ab6a351881a86dbff565c4db0085cc113/src/label.rs
    let encoding_name = b.unwrap_str().to_lowercase().replace("_", "-");

    // Supporting other encodings is tracked by #2282.
    if encoding_from_whatwg_label(&encoding_name).map(|e| e.name()) != Some("utf-8") {
        return Err(EvalError::InvalidEncodingName(encoding_name));
    }

    match str::from_utf8(a.unwrap_bytes()) {
        Ok(from) => Ok(Datum::String(from)),
        Err(e) => Err(EvalError::InvalidByteSequence {
            byte_sequence: e.to_string(),
            encoding_name,
        }),
    }
}

fn encode<'a>(
    bytes: Datum<'a>,
    format: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let format = encoding::lookup_format(format.unwrap_str())?;
    let out = format.encode(bytes.unwrap_bytes());
    Ok(Datum::from(temp_storage.push_string(out)))
}

fn decode<'a>(
    string: Datum<'a>,
    format: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let format = encoding::lookup_format(format.unwrap_str())?;
    let out = format.decode(string.unwrap_str())?;
    Ok(Datum::from(temp_storage.push_bytes(out)))
}

fn bit_length<'a, B>(bytes: B) -> Result<Datum<'a>, EvalError>
where
    B: AsRef<[u8]>,
{
    match i32::try_from(bytes.as_ref().len() * 8) {
        Ok(l) => Ok(Datum::from(l)),
        Err(_) => Err(EvalError::Int32OutOfRange),
    }
}

fn byte_length<'a, B>(bytes: B) -> Result<Datum<'a>, EvalError>
where
    B: AsRef<[u8]>,
{
    match i32::try_from(bytes.as_ref().len()) {
        Ok(l) => Ok(Datum::from(l)),
        Err(_) => Err(EvalError::Int32OutOfRange),
    }
}

fn char_length<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    match i32::try_from(a.unwrap_str().chars().count()) {
        Ok(l) => Ok(Datum::from(l)),
        Err(_) => Err(EvalError::Int32OutOfRange),
    }
}

fn encoded_bytes_char_length<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    // Convert PostgreSQL-style encoding names[1] to WHATWG-style encoding names[2],
    // which the encoding library uses[3].
    // [1]: https://www.postgresql.org/docs/9.5/multibyte.html
    // [2]: https://encoding.spec.whatwg.org/
    // [3]: https://github.com/lifthrasiir/rust-encoding/blob/4e79c35ab6a351881a86dbff565c4db0085cc113/src/label.rs
    let encoding_name = b.unwrap_str().to_lowercase().replace("_", "-");

    let enc = match encoding_from_whatwg_label(&encoding_name) {
        Some(enc) => enc,
        None => return Err(EvalError::InvalidEncodingName(encoding_name)),
    };

    let decoded_string = match enc.decode(a.unwrap_bytes(), DecoderTrap::Strict) {
        Ok(s) => s,
        Err(e) => {
            return Err(EvalError::InvalidByteSequence {
                byte_sequence: e.to_string(),
                encoding_name,
            })
        }
    };

    match i32::try_from(decoded_string.chars().count()) {
        Ok(l) => Ok(Datum::from(l)),
        Err(_) => Err(EvalError::Int32OutOfRange),
    }
}

fn sub_timestamp_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    add_timestamp_interval(a, Datum::Interval(-b.unwrap_interval()))
}

fn sub_timestamptz_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    add_timestamptz_interval(a, Datum::Interval(-b.unwrap_interval()))
}

fn add_timestamp_months(dt: NaiveDateTime, months: i32) -> NaiveDateTime {
    if months == 0 {
        return dt;
    }

    let mut months = months;

    let (mut year, mut month, mut day) = (dt.year(), dt.month0() as i32, dt.day());
    let years = months / 12;
    year += years;
    months %= 12;
    // positive modulus is easier to reason about
    if months < 0 {
        year -= 1;
        months += 12;
    }
    year += (month + months) / 12;
    month = (month + months) % 12;
    // account for dt.month0
    month += 1;

    // handle going from January 31st to February by saturation
    let mut new_d = chrono::NaiveDate::from_ymd_opt(year, month as u32, day);
    while new_d.is_none() {
        debug_assert!(day > 28, "there are no months with fewer than 28 days");
        day -= 1;
        new_d = chrono::NaiveDate::from_ymd_opt(year, month as u32, day);
    }
    let new_d = new_d.unwrap();

    // Neither postgres nor mysql support leap seconds, so this should be safe.
    //
    // Both my testing and https://dba.stackexchange.com/a/105829 support the
    // idea that we should ignore leap seconds
    new_d.and_hms_nano(dt.hour(), dt.minute(), dt.second(), dt.nanosecond())
}

fn add_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut cx = numeric::cx_datum();
    let mut a = a.unwrap_numeric().0;
    cx.add(&mut a, &b.unwrap_numeric().0);
    if cx.status().overflow() {
        Err(EvalError::FloatOverflow)
    } else {
        Ok(Datum::from(a))
    }
}

fn add_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_interval()
        .checked_add(&b.unwrap_interval())
        .ok_or(EvalError::IntervalOutOfRange)
        .map(Datum::from)
}

fn bit_and_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int16() & b.unwrap_int16())
}

fn bit_and_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int32() & b.unwrap_int32())
}

fn bit_and_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int64() & b.unwrap_int64())
}

fn bit_or_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int16() | b.unwrap_int16())
}

fn bit_or_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int32() | b.unwrap_int32())
}

fn bit_or_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int64() | b.unwrap_int64())
}

fn bit_xor_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int16() ^ b.unwrap_int16())
}

fn bit_xor_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int32() ^ b.unwrap_int32())
}

fn bit_xor_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int64() ^ b.unwrap_int64())
}

fn bit_shift_left_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    // widen to i32 and then cast back to i16 in order emulate the C promotion rules used in by Postgres
    // when the rhs in the 16-31 range, e.g. (1 << 17 should evaluate to 0)
    // see https://github.com/postgres/postgres/blob/REL_14_STABLE/src/backend/utils/adt/int.c#L1460-L1476
    let lhs: i32 = a.unwrap_int16() as i32;
    let rhs: u32 = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shl(rhs) as i16)
}

fn bit_shift_left_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_int32();
    let rhs = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shl(rhs))
}

fn bit_shift_left_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_int64();
    let rhs = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shl(rhs))
}

fn bit_shift_right_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    // widen to i32 and then cast back to i16 in order emulate the C promotion rules used in by Postgres
    // when the rhs in the 16-31 range, e.g. (-32767 >> 17 should evaluate to -1)
    // see https://github.com/postgres/postgres/blob/REL_14_STABLE/src/backend/utils/adt/int.c#L1460-L1476
    let lhs = a.unwrap_int16() as i32;
    let rhs = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shr(rhs) as i16)
}

fn bit_shift_right_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_int32();
    let rhs = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shr(rhs))
}

fn bit_shift_right_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_int64();
    let rhs = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shr(rhs))
}

fn sub_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int16()
        .checked_sub(b.unwrap_int16())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

fn sub_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int32()
        .checked_sub(b.unwrap_int32())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

fn sub_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int64()
        .checked_sub(b.unwrap_int64())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

fn sub_float32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_float32() - b.unwrap_float32())
}

fn sub_float64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_float64() - b.unwrap_float64())
}

fn sub_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut cx = numeric::cx_datum();
    let mut a = a.unwrap_numeric().0;
    cx.sub(&mut a, &b.unwrap_numeric().0);
    if cx.status().overflow() {
        Err(EvalError::FloatOverflow)
    } else {
        Ok(Datum::from(a))
    }
}

fn sub_timestamp<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_timestamp() - b.unwrap_timestamp())
}

fn sub_timestamptz<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_timestamptz() - b.unwrap_timestamptz())
}

fn sub_date<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from((a.unwrap_date() - b.unwrap_date()).num_days() as i32)
}

fn sub_time<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_time() - b.unwrap_time())
}

fn sub_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_interval()
        .checked_add(&-b.unwrap_interval())
        .ok_or(EvalError::IntervalOutOfRange)
        .map(Datum::from)
}

fn sub_date_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let date = a.unwrap_date();
    let interval = b.unwrap_interval();

    let dt = NaiveDate::from_ymd(date.year(), date.month(), date.day()).and_hms(0, 0, 0);
    let dt = add_timestamp_months(dt, -interval.months);
    Datum::Timestamp(dt - interval.duration_as_chrono())
}

fn sub_time_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let time = a.unwrap_time();
    let interval = b.unwrap_interval();
    let (t, _) = time.overflowing_sub_signed(interval.duration_as_chrono());
    Datum::Time(t)
}

fn mul_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int16()
        .checked_mul(b.unwrap_int16())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

fn mul_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int32()
        .checked_mul(b.unwrap_int32())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

fn mul_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int64()
        .checked_mul(b.unwrap_int64())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

fn mul_float32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_float32() * b.unwrap_float32())
}

fn mul_float64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_float64() * b.unwrap_float64())
}

fn mul_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut cx = numeric::cx_datum();
    let mut a = a.unwrap_numeric().0;
    cx.mul(&mut a, &b.unwrap_numeric().0);
    let cx_status = cx.status();
    if cx_status.overflow() {
        Err(EvalError::FloatOverflow)
    } else if cx_status.subnormal() {
        Err(EvalError::FloatUnderflow)
    } else {
        numeric::munge_numeric(&mut a).unwrap();
        Ok(Datum::from(a))
    }
}

fn mul_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_interval()
        .checked_mul(b.unwrap_float64())
        .ok_or(EvalError::IntervalOutOfRange)
        .map(Datum::from)
}

fn div_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int16();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_int16() / b))
    }
}

fn div_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int32();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_int32() / b))
    }
}

fn div_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int64();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_int64() / b))
    }
}

fn div_float32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float32();
    let b = b.unwrap_float32();
    if b == 0.0 && !a.is_nan() {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a / b))
    }
}

fn div_float64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float64();
    let b = b.unwrap_float64();
    if b == 0.0 && !a.is_nan() {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a / b))
    }
}

fn div_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut cx = numeric::cx_datum();
    let mut a = a.unwrap_numeric().0;
    let b = b.unwrap_numeric().0;

    cx.div(&mut a, &b);
    let cx_status = cx.status();

    // checking the status for division by zero errors is insufficient because
    // the underlying library treats 0/0 as undefined and not division by zero.
    if b.is_zero() {
        Err(EvalError::DivisionByZero)
    } else if cx_status.overflow() {
        Err(EvalError::FloatOverflow)
    } else if cx_status.subnormal() {
        Err(EvalError::FloatUnderflow)
    } else {
        numeric::munge_numeric(&mut a).unwrap();
        Ok(Datum::from(a))
    }
}

fn div_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_float64();
    if b == 0.0 {
        Err(EvalError::DivisionByZero)
    } else {
        a.unwrap_interval()
            .checked_div(b)
            .ok_or(EvalError::IntervalOutOfRange)
            .map(Datum::from)
    }
}

fn mod_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int16();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_int16() % b))
    }
}

fn mod_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int32();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_int32() % b))
    }
}

fn mod_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int64();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_int64() % b))
    }
}

fn mod_float32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_float32();
    if b == 0.0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_float32() % b))
    }
}

fn mod_float64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_float64();
    if b == 0.0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_float64() % b))
    }
}

fn mod_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric();
    let b = b.unwrap_numeric();
    if b.0.is_zero() {
        return Err(EvalError::DivisionByZero);
    }
    let mut cx = numeric::cx_datum();
    // Postgres does _not_ use IEEE 754-style remainder
    cx.rem(&mut a.0, &b.0);
    numeric::munge_numeric(&mut a.0).unwrap();
    Ok(Datum::Numeric(a))
}

fn neg_numeric<'a>(a: Datum<'a>) -> Datum<'a> {
    let mut a = a.unwrap_numeric();
    numeric::cx_datum().neg(&mut a.0);
    numeric::munge_numeric(&mut a.0).unwrap();
    Datum::Numeric(a)
}

pub fn neg_interval<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(-a.unwrap_interval())
}

fn sqrt_numeric<'a>(a: Datum<'a>) -> Result<Datum, EvalError> {
    let mut a = a.unwrap_numeric();
    if a.0.is_negative() {
        return Err(EvalError::NegSqrt);
    }
    let mut cx = numeric::cx_datum();
    cx.sqrt(&mut a.0);
    numeric::munge_numeric(&mut a.0).unwrap();
    Ok(Datum::Numeric(a))
}

fn log_guard_numeric(val: &Numeric, function_name: &str) -> Result<(), EvalError> {
    if val.is_negative() {
        return Err(EvalError::NegativeOutOfDomain(function_name.to_owned()));
    }
    if val.is_zero() {
        return Err(EvalError::ZeroOutOfDomain(function_name.to_owned()));
    }
    Ok(())
}

fn log_base_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric().0;
    log_guard_numeric(&a, "log")?;
    let mut b = b.unwrap_numeric().0;
    log_guard_numeric(&b, "log")?;
    let mut cx = numeric::cx_datum();
    cx.ln(&mut a);
    cx.ln(&mut b);
    cx.div(&mut b, &a);
    if a.is_zero() {
        Err(EvalError::DivisionByZero)
    } else {
        // This division can result in slightly wrong answers due to the
        // limitation of dividing irrational numbers. To correct that, see if
        // rounding off the value from its `numeric::NUMERIC_DATUM_MAX_PRECISION
        // - 1`th position results in an integral value.
        cx.set_precision(numeric::NUMERIC_DATUM_MAX_PRECISION - 1)
            .expect("reducing precision below max always succeeds");
        let mut integral_check = b.clone();

        // `reduce` rounds to the the context's final digit when the number of
        // digits in its argument exceeds its precision. We've contrived that to
        // happen by shrinking the context's precision by 1.
        cx.reduce(&mut integral_check);

        // Reduced integral values always have a non-negative exponent.
        let mut b = if integral_check.exponent() >= 0 {
            // We believe our result should have been an integral
            integral_check
        } else {
            b
        };

        numeric::munge_numeric(&mut b).unwrap();
        Ok(Datum::from(b))
    }
}

// From the `decNumber` library's documentation:
// > Inexact results will almost always be correctly rounded, but may be up to 1
// > ulp (unit in last place) in error in rare cases.
//
// See decNumberLog10 documentation at http://speleotrove.com/decimal/dnnumb.html
fn log_numeric<'a, 'b, F: Fn(&mut dec::Context<Numeric>, &mut Numeric)>(
    a: Datum<'a>,
    logic: F,
    name: &'b str,
) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric();
    log_guard_numeric(&a.0, name)?;
    let mut cx = numeric::cx_datum();
    logic(&mut cx, &mut a.0);
    numeric::munge_numeric(&mut a.0).unwrap();
    Ok(Datum::Numeric(a))
}

fn exp_numeric<'a>(a: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric();
    let mut cx = numeric::cx_datum();
    cx.exp(&mut a.0);
    let cx_status = cx.status();
    if cx_status.overflow() {
        Err(EvalError::FloatOverflow)
    } else if cx_status.subnormal() {
        Err(EvalError::FloatUnderflow)
    } else {
        numeric::munge_numeric(&mut a.0).unwrap();
        Ok(Datum::Numeric(a))
    }
}

fn power<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float64();
    let b = b.unwrap_float64();
    if a == 0.0 && b.is_sign_negative() {
        return Err(EvalError::Undefined(
            "zero raised to a negative power".to_owned(),
        ));
    }
    if a.is_sign_negative() && b.fract() != 0.0 {
        // Equivalent to PG error:
        // > a negative number raised to a non-integer power yields a complex result
        return Err(EvalError::ComplexOutOfRange("pow".to_owned()));
    }
    Ok(Datum::from(a.powf(b)))
}

fn power_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric().0;
    let b = b.unwrap_numeric().0;
    if a.is_zero() {
        if b.is_zero() {
            return Ok(Datum::from(Numeric::from(1)));
        }
        if b.is_negative() {
            return Err(EvalError::Undefined(
                "zero raised to a negative power".to_owned(),
            ));
        }
    }
    if a.is_negative() && b.exponent() < 0 {
        // Equivalent to PG error:
        // > a negative number raised to a non-integer power yields a complex result
        return Err(EvalError::ComplexOutOfRange("pow".to_owned()));
    }
    let mut cx = numeric::cx_datum();
    cx.pow(&mut a, &b);
    let cx_status = cx.status();
    if cx_status.overflow() || (cx_status.invalid_operation() && !b.is_negative()) {
        Err(EvalError::FloatOverflow)
    } else if cx_status.subnormal() || cx_status.invalid_operation() {
        Err(EvalError::FloatUnderflow)
    } else {
        numeric::munge_numeric(&mut a).unwrap();
        Ok(Datum::from(a))
    }
}

fn rescale_numeric<'a>(a: Datum<'a>, scale: u8) -> Result<Datum<'a>, EvalError> {
    let mut d = a.unwrap_numeric();
    if numeric::rescale(&mut d.0, scale).is_err() {
        return Err(EvalError::NumericFieldOverflow);
    };
    Ok(Datum::Numeric(d))
}

fn eq<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a == b)
}

fn not_eq<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a != b)
}

fn lt<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a < b)
}

fn lte<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a <= b)
}

fn gt<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a > b)
}

fn gte<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a >= b)
}

fn to_char_timestamp<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let fmt = DateTimeFormat::compile(b.unwrap_str());
    Datum::String(temp_storage.push_string(fmt.render(a.unwrap_timestamp())))
}

fn to_char_timestamptz<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let fmt = DateTimeFormat::compile(b.unwrap_str());
    Datum::String(temp_storage.push_string(fmt.render(a.unwrap_timestamptz())))
}

fn jsonb_get_int64<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
    stringify: bool,
) -> Datum<'a> {
    let i = b.unwrap_int64();
    match a {
        Datum::List(list) => {
            let i = if i >= 0 {
                i
            } else {
                // index backwards from the end
                (list.iter().count() as i64) + i
            };
            match list.iter().nth(i as usize) {
                Some(d) if stringify => jsonb_stringify(d, temp_storage),
                Some(d) => d,
                None => Datum::Null,
            }
        }
        Datum::Map(_) => Datum::Null,
        _ => {
            if i == 0 || i == -1 {
                // I have no idea why postgres does this, but we're stuck with it
                if stringify {
                    jsonb_stringify(a, temp_storage)
                } else {
                    a
                }
            } else {
                Datum::Null
            }
        }
    }
}

fn jsonb_get_string<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
    stringify: bool,
) -> Datum<'a> {
    let k = b.unwrap_str();
    match a {
        Datum::Map(dict) => match dict.iter().find(|(k2, _v)| k == *k2) {
            Some((_k, v)) if stringify => jsonb_stringify(v, temp_storage),
            Some((_k, v)) => v,
            None => Datum::Null,
        },
        _ => Datum::Null,
    }
}

fn jsonb_get_path<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
    stringify: bool,
) -> Datum<'a> {
    let mut json = a;
    let path = b.unwrap_array().elements();
    for key in path.iter() {
        let key = match key {
            Datum::String(s) => s,
            Datum::Null => return Datum::Null,
            _ => unreachable!("keys in jsonb_get_path known to be strings"),
        };
        json = match json {
            Datum::Map(map) => match map.iter().find(|(k, _)| key == *k) {
                Some((_k, v)) => v,
                None => return Datum::Null,
            },
            Datum::List(list) => match strconv::parse_int64(key) {
                Ok(i) => {
                    let i = if i >= 0 {
                        i
                    } else {
                        // index backwards from the end
                        (list.iter().count() as i64) + i
                    };
                    list.iter().nth(i as usize).unwrap_or(Datum::Null)
                }
                Err(_) => return Datum::Null,
            },
            _ => return Datum::Null,
        }
    }
    if stringify {
        jsonb_stringify(json, temp_storage)
    } else {
        json
    }
}

fn jsonb_contains_string<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let k = b.unwrap_str();
    // https://www.postgresql.org/docs/current/datatype-json.html#JSON-CONTAINMENT
    match a {
        Datum::List(list) => list.iter().any(|k2| b == k2).into(),
        Datum::Map(dict) => dict.iter().any(|(k2, _v)| k == k2).into(),
        Datum::String(string) => (string == k).into(),
        _ => false.into(),
    }
}

fn map_contains_key<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let map = a.unwrap_map();
    let k = b.unwrap_str(); // Map keys are always text.
    map.iter().any(|(k2, _v)| k == k2).into()
}

fn map_contains_all_keys<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let map = a.unwrap_map();
    let keys = b.unwrap_array();

    keys.elements()
        .iter()
        .all(|key| !key.is_null() && map.iter().any(|(k, _v)| k == key.unwrap_str()))
        .into()
}

fn map_contains_any_keys<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let map = a.unwrap_map();
    let keys = b.unwrap_array();

    keys.elements()
        .iter()
        .any(|key| !key.is_null() && map.iter().any(|(k, _v)| k == key.unwrap_str()))
        .into()
}

fn map_contains_map<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let map_a = a.unwrap_map();
    b.unwrap_map()
        .iter()
        .all(|(b_key, b_val)| {
            map_a
                .iter()
                .any(|(a_key, a_val)| (a_key == b_key) && (a_val == b_val))
        })
        .into()
}

fn map_get_value<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let target_key = b.unwrap_str();
    match a.unwrap_map().iter().find(|(key, _v)| target_key == *key) {
        Some((_k, v)) => v,
        None => Datum::Null,
    }
}

fn map_get_values<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let map = a.unwrap_map();
    let values: Vec<Datum> = b
        .unwrap_array()
        .elements()
        .iter()
        .map(
            |target_key| match map.iter().find(|(key, _v)| target_key.unwrap_str() == *key) {
                Some((_k, v)) => v,
                None => Datum::Null,
            },
        )
        .collect();

    temp_storage.make_datum(|packer| {
        packer
            .push_array(
                &[ArrayDimension {
                    lower_bound: 1,
                    length: values.len(),
                }],
                values,
            )
            .unwrap()
    })
}

// TODO(jamii) nested loops are possibly not the fastest way to do this
fn jsonb_contains_jsonb<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    // https://www.postgresql.org/docs/current/datatype-json.html#JSON-CONTAINMENT
    fn contains(a: Datum, b: Datum, at_top_level: bool) -> bool {
        match (a, b) {
            (Datum::JsonNull, Datum::JsonNull) => true,
            (Datum::False, Datum::False) => true,
            (Datum::True, Datum::True) => true,
            (Datum::Int64(a), Datum::Int64(b)) => (a == b),
            (Datum::Int64(a), Datum::Float64(b)) => (OrderedFloat(a as f64) == b),
            (Datum::Float64(a), Datum::Int64(b)) => (a == OrderedFloat(b as f64)),
            (Datum::Float64(a), Datum::Float64(b)) => (a == b),
            (Datum::String(a), Datum::String(b)) => (a == b),
            (Datum::List(a), Datum::List(b)) => b
                .iter()
                .all(|b_elem| a.iter().any(|a_elem| contains(a_elem, b_elem, false))),
            (Datum::Map(a), Datum::Map(b)) => b.iter().all(|(b_key, b_val)| {
                a.iter()
                    .any(|(a_key, a_val)| (a_key == b_key) && contains(a_val, b_val, false))
            }),

            // fun special case
            (Datum::List(a), b) => {
                at_top_level && a.iter().any(|a_elem| contains(a_elem, b, false))
            }

            _ => false,
        }
    }
    contains(a, b, true).into()
}

fn jsonb_concat<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    match (a, b) {
        (Datum::Map(dict_a), Datum::Map(dict_b)) => {
            let mut pairs = dict_b.iter().chain(dict_a.iter()).collect::<Vec<_>>();
            // stable sort, so if keys collide dedup prefers dict_b
            pairs.sort_by(|(k1, _v1), (k2, _v2)| k1.cmp(k2));
            pairs.dedup_by(|(k1, _v1), (k2, _v2)| k1 == k2);
            temp_storage.make_datum(|packer| packer.push_dict(pairs))
        }
        (Datum::List(list_a), Datum::List(list_b)) => {
            let elems = list_a.iter().chain(list_b.iter());
            temp_storage.make_datum(|packer| packer.push_list(elems))
        }
        (Datum::List(list_a), b) => {
            let elems = list_a.iter().chain(Some(b).into_iter());
            temp_storage.make_datum(|packer| packer.push_list(elems))
        }
        (a, Datum::List(list_b)) => {
            let elems = Some(a).into_iter().chain(list_b.iter());
            temp_storage.make_datum(|packer| packer.push_list(elems))
        }
        _ => Datum::Null,
    }
}

fn jsonb_delete_int64<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let i = b.unwrap_int64();
    match a {
        Datum::List(list) => {
            let i = if i >= 0 {
                i
            } else {
                // index backwards from the end
                (list.iter().count() as i64) + i
            } as usize;
            let elems = list
                .iter()
                .enumerate()
                .filter(|(i2, _e)| i != *i2)
                .map(|(_, e)| e);
            temp_storage.make_datum(|packer| packer.push_list(elems))
        }
        _ => Datum::Null,
    }
}

fn jsonb_delete_string<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    match a {
        Datum::List(list) => {
            let elems = list.iter().filter(|e| b != *e);
            temp_storage.make_datum(|packer| packer.push_list(elems))
        }
        Datum::Map(dict) => {
            let k = b.unwrap_str();
            let pairs = dict.iter().filter(|(k2, _v)| k != *k2);
            temp_storage.make_datum(|packer| packer.push_dict(pairs))
        }
        _ => Datum::Null,
    }
}

fn ascii<'a>(a: Datum<'a>) -> Datum<'a> {
    match a.unwrap_str().chars().next() {
        None => Datum::Int32(0),
        Some(v) => Datum::Int32(v as i32),
    }
}

/// A timestamp with both a date and a time component, but not necessarily a
/// timezone component.
pub trait TimestampLike: chrono::Datelike + chrono::Timelike + for<'a> Into<Datum<'a>> {
    fn new(date: NaiveDate, time: NaiveTime) -> Self;

    /// Returns the weekday as a `usize` between 0 and 6, where 0 represents
    /// Sunday and 6 represents Saturday.
    fn weekday0(&self) -> usize {
        self.weekday().num_days_from_sunday() as usize
    }

    /// Like [`chrono::Datelike::year_ce`], but works on the ISO week system.
    fn iso_year_ce(&self) -> u32 {
        let year = self.iso_week().year();
        if year < 1 {
            (1 - year) as u32
        } else {
            year as u32
        }
    }

    fn timestamp(&self) -> i64;

    fn timestamp_subsec_micros(&self) -> u32;

    fn extract_epoch(&self) -> f64 {
        self.timestamp() as f64 + (self.timestamp_subsec_micros() as f64) / 1e6
    }

    fn extract_year(&self) -> f64 {
        f64::from(self.year())
    }

    fn extract_quarter(&self) -> f64 {
        (f64::from(self.month()) / 3.0).ceil()
    }

    fn extract_month(&self) -> f64 {
        f64::from(self.month())
    }

    fn extract_day(&self) -> f64 {
        f64::from(self.day())
    }

    fn extract_hour(&self) -> f64 {
        f64::from(self.hour())
    }

    fn extract_minute(&self) -> f64 {
        f64::from(self.minute())
    }

    fn extract_second(&self) -> f64 {
        let s = f64::from(self.second());
        let ns = f64::from(self.nanosecond()) / 1e9;
        s + ns
    }

    fn extract_millisecond(&self) -> f64 {
        let s = f64::from(self.second() * 1_000);
        let ns = f64::from(self.nanosecond()) / 1e6;
        s + ns
    }

    fn extract_microsecond(&self) -> f64 {
        let s = f64::from(self.second() * 1_000_000);
        let ns = f64::from(self.nanosecond()) / 1e3;
        s + ns
    }

    fn extract_millennium(&self) -> f64 {
        f64::from((self.year() + if self.year() > 0 { 999 } else { -1_000 }) / 1_000)
    }

    fn extract_century(&self) -> f64 {
        f64::from((self.year() + if self.year() > 0 { 99 } else { -100 }) / 100)
    }

    fn extract_decade(&self) -> f64 {
        f64::from(self.year().div_euclid(10))
    }

    /// Extract the iso week of the year
    ///
    /// Note that because isoweeks are defined in terms of January 4th, Jan 1 is only in week
    /// 1 about half of the time
    fn extract_week(&self) -> f64 {
        f64::from(self.iso_week().week())
    }

    fn extract_dayofyear(&self) -> f64 {
        f64::from(self.ordinal())
    }

    fn extract_dayofweek(&self) -> f64 {
        f64::from(self.weekday().num_days_from_sunday())
    }

    fn extract_isodayofweek(&self) -> f64 {
        f64::from(self.weekday().number_from_monday())
    }

    fn truncate_microseconds(&self) -> Self {
        let time = NaiveTime::from_hms_micro(
            self.hour(),
            self.minute(),
            self.second(),
            self.nanosecond() / 1_000,
        );

        Self::new(self.date(), time)
    }

    fn truncate_milliseconds(&self) -> Self {
        let time = NaiveTime::from_hms_milli(
            self.hour(),
            self.minute(),
            self.second(),
            self.nanosecond() / 1_000_000,
        );

        Self::new(self.date(), time)
    }

    fn truncate_second(&self) -> Self {
        let time = NaiveTime::from_hms(self.hour(), self.minute(), self.second());

        Self::new(self.date(), time)
    }

    fn truncate_minute(&self) -> Self {
        Self::new(
            self.date(),
            NaiveTime::from_hms(self.hour(), self.minute(), 0),
        )
    }

    fn truncate_hour(&self) -> Self {
        Self::new(self.date(), NaiveTime::from_hms(self.hour(), 0, 0))
    }

    fn truncate_day(&self) -> Self {
        Self::new(self.date(), NaiveTime::from_hms(0, 0, 0))
    }

    fn truncate_week(&self) -> Result<Self, EvalError> {
        let num_days_from_monday = self.date().weekday().num_days_from_monday() as i64;
        let new_date = NaiveDate::from_ymd(self.year(), self.month(), self.day())
            .checked_sub_signed(Duration::days(num_days_from_monday))
            .ok_or(EvalError::TimestampOutOfRange)?;
        Ok(Self::new(new_date, NaiveTime::from_hms(0, 0, 0)))
    }

    fn truncate_month(&self) -> Self {
        Self::new(
            NaiveDate::from_ymd(self.year(), self.month(), 1),
            NaiveTime::from_hms(0, 0, 0),
        )
    }

    fn truncate_quarter(&self) -> Self {
        let month = self.month();
        let quarter = if month <= 3 {
            1
        } else if month <= 6 {
            4
        } else if month <= 9 {
            7
        } else {
            10
        };

        Self::new(
            NaiveDate::from_ymd(self.year(), quarter, 1),
            NaiveTime::from_hms(0, 0, 0),
        )
    }

    fn truncate_year(&self) -> Self {
        Self::new(
            NaiveDate::from_ymd(self.year(), 1, 1),
            NaiveTime::from_hms(0, 0, 0),
        )
    }
    fn truncate_decade(&self) -> Self {
        Self::new(
            NaiveDate::from_ymd(self.year() - self.year().rem_euclid(10), 1, 1),
            NaiveTime::from_hms(0, 0, 0),
        )
    }
    fn truncate_century(&self) -> Self {
        // Expects the first year of the century, meaning 2001 instead of 2000.
        Self::new(
            NaiveDate::from_ymd(
                if self.year() > 0 {
                    self.year() - (self.year() - 1) % 100
                } else {
                    self.year() - self.year() % 100 - 99
                },
                1,
                1,
            ),
            NaiveTime::from_hms(0, 0, 0),
        )
    }
    fn truncate_millennium(&self) -> Self {
        // Expects the first year of the millennium, meaning 2001 instead of 2000.
        Self::new(
            NaiveDate::from_ymd(
                if self.year() > 0 {
                    self.year() - (self.year() - 1) % 1000
                } else {
                    self.year() - self.year() % 1000 - 999
                },
                1,
                1,
            ),
            NaiveTime::from_hms(0, 0, 0),
        )
    }

    /// Return the date component of the timestamp
    fn date(&self) -> NaiveDate;

    /// Returns a string representing the timezone's offset from UTC.
    fn timezone_offset(&self) -> &'static str;

    /// Returns a string representing the hour portion of the timezone's offset
    /// from UTC.
    fn timezone_hours(&self) -> &'static str;

    /// Returns a string representing the minute portion of the timezone's
    /// offset from UTC.
    fn timezone_minutes(&self) -> &'static str;

    /// Returns the abbreviated name of the timezone with the specified
    /// capitalization.
    fn timezone_name(&self, caps: bool) -> &'static str;
}

impl TimestampLike for chrono::NaiveDateTime {
    fn new(date: NaiveDate, time: NaiveTime) -> Self {
        NaiveDateTime::new(date, time)
    }

    fn date(&self) -> NaiveDate {
        self.date()
    }

    fn timestamp(&self) -> i64 {
        self.timestamp()
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        self.timestamp_subsec_micros()
    }

    fn timezone_offset(&self) -> &'static str {
        "+00"
    }

    fn timezone_hours(&self) -> &'static str {
        "+00"
    }

    fn timezone_minutes(&self) -> &'static str {
        "00"
    }

    fn timezone_name(&self, _caps: bool) -> &'static str {
        ""
    }
}

impl TimestampLike for chrono::DateTime<chrono::Utc> {
    fn new(date: NaiveDate, time: NaiveTime) -> Self {
        DateTime::<Utc>::from_utc(NaiveDateTime::new(date, time), Utc)
    }

    fn date(&self) -> NaiveDate {
        self.naive_utc().date()
    }

    fn timestamp(&self) -> i64 {
        self.timestamp()
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        self.timestamp_subsec_micros()
    }

    fn timezone_offset(&self) -> &'static str {
        "+00"
    }

    fn timezone_hours(&self) -> &'static str {
        "+00"
    }

    fn timezone_minutes(&self) -> &'static str {
        "00"
    }

    fn timezone_name(&self, caps: bool) -> &'static str {
        if caps {
            "UTC"
        } else {
            "utc"
        }
    }
}

fn date_part_interval<'a>(a: Datum<'a>, interval: Interval) -> Result<Datum<'a>, EvalError> {
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => date_part_interval_inner(units, interval),
        Err(_) => Err(EvalError::UnknownUnits(units.to_owned())),
    }
}

fn date_part_interval_inner(
    units: DateTimeUnits,
    interval: Interval,
) -> Result<Datum<'static>, EvalError> {
    match units {
        DateTimeUnits::Epoch => Ok(interval.as_seconds().into()),
        DateTimeUnits::Year => Ok(interval.years().into()),
        DateTimeUnits::Day => Ok(interval.days().into()),
        DateTimeUnits::Hour => Ok(interval.hours().into()),
        DateTimeUnits::Minute => Ok(interval.minutes().into()),
        DateTimeUnits::Second => Ok(interval.seconds().into()),
        DateTimeUnits::Millennium => Ok(interval.millennia().into()),
        DateTimeUnits::Century => Ok(interval.centuries().into()),
        DateTimeUnits::Decade => Ok(interval.decades().into()),
        DateTimeUnits::Quarter => Ok(interval.quarters().into()),
        DateTimeUnits::Month => Ok(interval.months().into()),
        DateTimeUnits::Milliseconds => Ok(interval.milliseconds().into()),
        DateTimeUnits::Microseconds => Ok(interval.microseconds().into()),
        DateTimeUnits::Week
        | DateTimeUnits::Timezone
        | DateTimeUnits::TimezoneHour
        | DateTimeUnits::TimezoneMinute
        | DateTimeUnits::DayOfWeek
        | DateTimeUnits::DayOfYear
        | DateTimeUnits::IsoDayOfWeek
        | DateTimeUnits::IsoDayOfYear => Err(EvalError::UnsupportedDateTimeUnits(units)),
    }
}

fn date_part_timestamp<'a, T>(a: Datum<'a>, ts: T) -> Result<Datum<'a>, EvalError>
where
    T: TimestampLike,
{
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => date_part_timestamp_inner(units, ts),
        Err(_) => Err(EvalError::UnknownUnits(units.to_owned())),
    }
}

fn date_part_timestamp_inner<'a, T>(units: DateTimeUnits, ts: T) -> Result<Datum<'a>, EvalError>
where
    T: TimestampLike,
{
    match units {
        DateTimeUnits::Epoch => Ok(ts.extract_epoch().into()),
        DateTimeUnits::Year => Ok(ts.extract_year().into()),
        DateTimeUnits::Quarter => Ok(ts.extract_quarter().into()),
        DateTimeUnits::Week => Ok(ts.extract_week().into()),
        DateTimeUnits::Day => Ok(ts.extract_day().into()),
        DateTimeUnits::DayOfWeek => Ok(ts.extract_dayofweek().into()),
        DateTimeUnits::DayOfYear => Ok(ts.extract_dayofyear().into()),
        DateTimeUnits::IsoDayOfWeek => Ok(ts.extract_isodayofweek().into()),
        DateTimeUnits::Hour => Ok(ts.extract_hour().into()),
        DateTimeUnits::Minute => Ok(ts.extract_minute().into()),
        DateTimeUnits::Second => Ok(ts.extract_second().into()),
        DateTimeUnits::Month => Ok(ts.extract_month().into()),
        DateTimeUnits::Milliseconds => Ok(ts.extract_millisecond().into()),
        DateTimeUnits::Microseconds => Ok(ts.extract_microsecond().into()),
        DateTimeUnits::Millennium => Ok(ts.extract_millennium().into()),
        DateTimeUnits::Century => Ok(ts.extract_century().into()),
        DateTimeUnits::Decade => Ok(ts.extract_decade().into()),
        DateTimeUnits::Timezone
        | DateTimeUnits::TimezoneHour
        | DateTimeUnits::TimezoneMinute
        | DateTimeUnits::IsoDayOfYear => Err(EvalError::UnsupportedDateTimeUnits(units)),
    }
}

fn date_trunc<'a, T>(a: Datum<'a>, ts: T) -> Result<Datum<'a>, EvalError>
where
    T: TimestampLike,
{
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => date_trunc_inner(units, ts),
        Err(_) => Err(EvalError::UnknownUnits(units.to_owned())),
    }
}

fn date_trunc_inner<'a, T>(units: DateTimeUnits, ts: T) -> Result<Datum<'a>, EvalError>
where
    T: TimestampLike,
{
    match units {
        DateTimeUnits::Millennium => Ok(ts.truncate_millennium().into()),
        DateTimeUnits::Century => Ok(ts.truncate_century().into()),
        DateTimeUnits::Decade => Ok(ts.truncate_decade().into()),
        DateTimeUnits::Year => Ok(ts.truncate_year().into()),
        DateTimeUnits::Quarter => Ok(ts.truncate_quarter().into()),
        DateTimeUnits::Week => Ok(ts.truncate_week()?.into()),
        DateTimeUnits::Day => Ok(ts.truncate_day().into()),
        DateTimeUnits::Hour => Ok(ts.truncate_hour().into()),
        DateTimeUnits::Minute => Ok(ts.truncate_minute().into()),
        DateTimeUnits::Second => Ok(ts.truncate_second().into()),
        DateTimeUnits::Month => Ok(ts.truncate_month().into()),
        DateTimeUnits::Milliseconds => Ok(ts.truncate_milliseconds().into()),
        DateTimeUnits::Microseconds => Ok(ts.truncate_microseconds().into()),
        DateTimeUnits::Epoch
        | DateTimeUnits::Timezone
        | DateTimeUnits::TimezoneHour
        | DateTimeUnits::TimezoneMinute
        | DateTimeUnits::DayOfWeek
        | DateTimeUnits::DayOfYear
        | DateTimeUnits::IsoDayOfWeek
        | DateTimeUnits::IsoDayOfYear => Err(EvalError::UnsupportedDateTimeUnits(units)),
    }
}

/// Parses a named timezone like `EST` or `America/New_York`, or a fixed-offset timezone like `-05:00`.
pub(crate) fn parse_timezone(tz: &str) -> Result<Timezone, EvalError> {
    tz.parse()
        .map_err(|_| EvalError::InvalidTimezone(tz.to_owned()))
}

/// Converts the time `t`, which is assumed to be in UTC, to the timezone `tz`.
/// For example, `EST` and `17:39:14` would return `12:39:14`.
fn timezone_time(tz: Timezone, t: NaiveTime, wall_time: &NaiveDateTime) -> Datum<'static> {
    let offset = match tz {
        Timezone::FixedOffset(offset) => offset,
        Timezone::Tz(tz) => tz.offset_from_utc_datetime(&wall_time).fix(),
    };
    (t + offset).into()
}

/// Converts the timestamp `dt`, which is assumed to be in the time of the timezone `tz` to a timestamptz in UTC.
/// This operation is fallible because certain timestamps at timezones that observe DST are simply impossible or
/// ambiguous. In case of ambiguity (when a hour repeats) we will prefer the latest variant, and when an hour is
/// impossible, we will attempt to fix it by advancing it. For example, `EST` and `2020-11-11T12:39:14` would return
/// `2020-11-11T17:39:14Z`. A DST observing timezone like `America/New_York` would cause the following DST anomalies:
/// `2020-11-01T00:59:59` -> `2020-11-01T04:59:59Z` and `2020-11-01T01:00:00` -> `2020-11-01T06:00:00Z`
/// `2020-03-08T02:59:59` -> `2020-03-08T07:59:59Z` and `2020-03-08T03:00:00` -> `2020-03-08T07:00:00Z`
fn timezone_timestamp(tz: Timezone, mut dt: NaiveDateTime) -> Result<Datum<'static>, EvalError> {
    let offset = match tz {
        Timezone::FixedOffset(offset) => offset,
        Timezone::Tz(tz) => match tz.offset_from_local_datetime(&dt).latest() {
            Some(offset) => offset.fix(),
            None => {
                dt += Duration::hours(1);
                tz.offset_from_local_datetime(&dt)
                    .latest()
                    .ok_or(EvalError::InvalidTimezoneConversion)?
                    .fix()
            }
        },
    };
    Ok(DateTime::from_utc(dt - offset, Utc).into())
}

/// Converts the UTC timestamptz `utc` to the local timestamp of the timezone `tz`.
/// For example, `EST` and `2020-11-11T17:39:14Z` would return `2020-11-11T12:39:14`.
fn timezone_timestamptz(tz: Timezone, utc: DateTime<Utc>) -> Datum<'static> {
    let offset = match tz {
        Timezone::FixedOffset(offset) => offset,
        Timezone::Tz(tz) => tz.offset_from_utc_datetime(&utc.naive_utc()).fix(),
    };
    (utc.naive_utc() + offset).into()
}

/// Converts the time datum `b`, which is assumed to be in UTC, to the timezone that the interval datum `a` is assumed
/// to represent. The interval is not allowed to hold months, but there are no limits on the amount of seconds.
/// The interval acts like a `chrono::FixedOffset`, without the `-86,400 < x < 86,400` limitation.
fn timezone_interval_time(a: Datum<'_>, b: Datum<'_>) -> Result<Datum<'static>, EvalError> {
    let interval = a.unwrap_interval();
    if interval.months != 0 {
        Err(EvalError::InvalidTimezoneInterval)
    } else {
        Ok(b.unwrap_time()
            .overflowing_add_signed(interval.duration_as_chrono())
            .0
            .into())
    }
}

/// Converts the timestamp datum `b`, which is assumed to be in the time of the timezone datum `a` to a timestamptz
/// in UTC. The interval is not allowed to hold months, but there are no limits on the amount of seconds.
/// The interval acts like a `chrono::FixedOffset`, without the `-86,400 < x < 86,400` limitation.
fn timezone_interval_timestamp(a: Datum<'_>, b: Datum<'_>) -> Result<Datum<'static>, EvalError> {
    let interval = a.unwrap_interval();
    if interval.months != 0 {
        Err(EvalError::InvalidTimezoneInterval)
    } else {
        Ok(DateTime::from_utc(b.unwrap_timestamp() - interval.duration_as_chrono(), Utc).into())
    }
}

/// Converts the UTC timestamptz datum `b`, to the local timestamp of the timezone datum `a`.
/// The interval is not allowed to hold months, but there are no limits on the amount of seconds.
/// The interval acts like a `chrono::FixedOffset`, without the `-86,400 < x < 86,400` limitation.
fn timezone_interval_timestamptz(a: Datum<'_>, b: Datum<'_>) -> Result<Datum<'static>, EvalError> {
    let interval = a.unwrap_interval();
    if interval.months != 0 {
        Err(EvalError::InvalidTimezoneInterval)
    } else {
        Ok((b.unwrap_timestamptz().naive_utc() + interval.duration_as_chrono()).into())
    }
}

fn jsonb_array_length<'a>(a: Datum<'a>) -> Datum<'a> {
    match a {
        Datum::List(list) => Datum::Int64(list.iter().count() as i64),
        _ => Datum::Null,
    }
}

fn jsonb_typeof<'a>(a: Datum<'a>) -> Datum<'a> {
    match a {
        Datum::Map(_) => Datum::String("object"),
        Datum::List(_) => Datum::String("array"),
        Datum::String(_) => Datum::String("string"),
        Datum::Float64(_) => Datum::String("number"),
        Datum::Int64(_) => Datum::String("number"),
        Datum::True | Datum::False => Datum::String("boolean"),
        Datum::JsonNull => Datum::String("null"),
        Datum::Null => Datum::Null,
        _ => panic!("Not jsonb: {:?}", a),
    }
}

fn jsonb_strip_nulls<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    fn strip_nulls(a: Datum, row: &mut Row) {
        match a {
            Datum::Map(dict) => row.push_dict_with(|row| {
                for (k, v) in dict.iter() {
                    match v {
                        Datum::JsonNull => (),
                        _ => {
                            row.push(Datum::String(k));
                            strip_nulls(v, row);
                        }
                    }
                }
            }),
            Datum::List(list) => row.push_list_with(|row| {
                for elem in list.iter() {
                    strip_nulls(elem, row);
                }
            }),
            _ => row.push(a),
        }
    }
    temp_storage.make_datum(|row| strip_nulls(a, row))
}

fn jsonb_pretty<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    strconv::format_jsonb_pretty(&mut buf, JsonbRef::from_datum(a));
    Datum::String(temp_storage.push_string(buf))
}

#[derive(
    Ord, PartialOrd, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash, MzEnumReflect,
)]
pub enum BinaryFunc {
    And,
    Or,
    AddInt16,
    AddInt32,
    AddInt64,
    AddFloat32,
    AddFloat64,
    AddInterval,
    AddTimestampInterval,
    AddTimestampTzInterval,
    AddDateInterval,
    AddDateTime,
    AddTimeInterval,
    AddNumeric,
    BitAndInt16,
    BitAndInt32,
    BitAndInt64,
    BitOrInt16,
    BitOrInt32,
    BitOrInt64,
    BitXorInt16,
    BitXorInt32,
    BitXorInt64,
    BitShiftLeftInt16,
    BitShiftLeftInt32,
    BitShiftLeftInt64,
    BitShiftRightInt16,
    BitShiftRightInt32,
    BitShiftRightInt64,
    SubInt16,
    SubInt32,
    SubInt64,
    SubFloat32,
    SubFloat64,
    SubInterval,
    SubTimestamp,
    SubTimestampTz,
    SubTimestampInterval,
    SubTimestampTzInterval,
    SubDate,
    SubDateInterval,
    SubTime,
    SubTimeInterval,
    SubNumeric,
    MulInt16,
    MulInt32,
    MulInt64,
    MulFloat32,
    MulFloat64,
    MulNumeric,
    MulInterval,
    DivInt16,
    DivInt32,
    DivInt64,
    DivFloat32,
    DivFloat64,
    DivNumeric,
    DivInterval,
    ModInt16,
    ModInt32,
    ModInt64,
    ModFloat32,
    ModFloat64,
    ModNumeric,
    RoundNumeric,
    Eq,
    NotEq,
    Lt,
    Lte,
    Gt,
    Gte,
    IsLikePatternMatch { case_insensitive: bool },
    IsRegexpMatch { case_insensitive: bool },
    ToCharTimestamp,
    ToCharTimestampTz,
    DatePartInterval,
    DatePartTimestamp,
    DatePartTimestampTz,
    DateTruncTimestamp,
    DateTruncTimestampTz,
    TimezoneTimestamp,
    TimezoneTimestampTz,
    TimezoneTime { wall_time: NaiveDateTime },
    TimezoneIntervalTimestamp,
    TimezoneIntervalTimestampTz,
    TimezoneIntervalTime,
    TextConcat,
    JsonbGetInt64 { stringify: bool },
    JsonbGetString { stringify: bool },
    JsonbGetPath { stringify: bool },
    JsonbContainsString,
    JsonbConcat,
    JsonbContainsJsonb,
    JsonbDeleteInt64,
    JsonbDeleteString,
    MapContainsKey,
    MapGetValue,
    MapGetValues,
    MapContainsAllKeys,
    MapContainsAnyKeys,
    MapContainsMap,
    ConvertFrom,
    Left,
    Position,
    Right,
    RepeatString,
    Trim,
    TrimLeading,
    TrimTrailing,
    EncodedBytesCharLength,
    ListIndex,
    ListLengthMax { max_dim: usize },
    ArrayContains,
    ArrayIndex,
    ArrayLength,
    ArrayLower,
    ArrayUpper,
    ListListConcat,
    ListElementConcat,
    ElementListConcat,
    DigestString,
    DigestBytes,
    MzRenderTypemod,
    Encode,
    Decode,
    LogNumeric,
    Power,
    PowerNumeric,
}

impl BinaryFunc {
    pub fn eval<'a>(
        &'a self,
        datums: &[Datum<'a>],
        temp_storage: &'a RowArena,
        a_expr: &'a MirScalarExpr,
        b_expr: &'a MirScalarExpr,
    ) -> Result<Datum<'a>, EvalError> {
        macro_rules! eager {
            ($func:expr $(, $args:expr)*) => {{
                let a = a_expr.eval(datums, temp_storage)?;
                let b = b_expr.eval(datums, temp_storage)?;
                if self.propagates_nulls() && (a.is_null() || b.is_null()) {
                    return Ok(Datum::Null);
                }
                $func(a, b $(, $args)*)
            }}
        }

        match self {
            BinaryFunc::And => and(datums, temp_storage, a_expr, b_expr),
            BinaryFunc::Or => or(datums, temp_storage, a_expr, b_expr),
            BinaryFunc::AddInt16 => eager!(add_int16),
            BinaryFunc::AddInt32 => eager!(add_int32),
            BinaryFunc::AddInt64 => eager!(add_int64),
            BinaryFunc::AddFloat32 => Ok(eager!(add_float32)),
            BinaryFunc::AddFloat64 => Ok(eager!(add_float64)),
            BinaryFunc::AddTimestampInterval => Ok(eager!(add_timestamp_interval)),
            BinaryFunc::AddTimestampTzInterval => Ok(eager!(add_timestamptz_interval)),
            BinaryFunc::AddDateTime => Ok(eager!(add_date_time)),
            BinaryFunc::AddDateInterval => Ok(eager!(add_date_interval)),
            BinaryFunc::AddTimeInterval => Ok(eager!(add_time_interval)),
            BinaryFunc::AddNumeric => eager!(add_numeric),
            BinaryFunc::AddInterval => eager!(add_interval),
            BinaryFunc::BitAndInt16 => Ok(eager!(bit_and_int16)),
            BinaryFunc::BitAndInt32 => Ok(eager!(bit_and_int32)),
            BinaryFunc::BitAndInt64 => Ok(eager!(bit_and_int64)),
            BinaryFunc::BitOrInt16 => Ok(eager!(bit_or_int16)),
            BinaryFunc::BitOrInt32 => Ok(eager!(bit_or_int32)),
            BinaryFunc::BitOrInt64 => Ok(eager!(bit_or_int64)),
            BinaryFunc::BitXorInt16 => Ok(eager!(bit_xor_int16)),
            BinaryFunc::BitXorInt32 => Ok(eager!(bit_xor_int32)),
            BinaryFunc::BitXorInt64 => Ok(eager!(bit_xor_int64)),
            BinaryFunc::BitShiftLeftInt16 => Ok(eager!(bit_shift_left_int16)),
            BinaryFunc::BitShiftLeftInt32 => Ok(eager!(bit_shift_left_int32)),
            BinaryFunc::BitShiftLeftInt64 => Ok(eager!(bit_shift_left_int64)),
            BinaryFunc::BitShiftRightInt16 => Ok(eager!(bit_shift_right_int16)),
            BinaryFunc::BitShiftRightInt32 => Ok(eager!(bit_shift_right_int32)),
            BinaryFunc::BitShiftRightInt64 => Ok(eager!(bit_shift_right_int64)),
            BinaryFunc::SubInt16 => eager!(sub_int16),
            BinaryFunc::SubInt32 => eager!(sub_int32),
            BinaryFunc::SubInt64 => eager!(sub_int64),
            BinaryFunc::SubFloat32 => Ok(eager!(sub_float32)),
            BinaryFunc::SubFloat64 => Ok(eager!(sub_float64)),
            BinaryFunc::SubTimestamp => Ok(eager!(sub_timestamp)),
            BinaryFunc::SubTimestampTz => Ok(eager!(sub_timestamptz)),
            BinaryFunc::SubTimestampInterval => Ok(eager!(sub_timestamp_interval)),
            BinaryFunc::SubTimestampTzInterval => Ok(eager!(sub_timestamptz_interval)),
            BinaryFunc::SubInterval => eager!(sub_interval),
            BinaryFunc::SubDate => Ok(eager!(sub_date)),
            BinaryFunc::SubDateInterval => Ok(eager!(sub_date_interval)),
            BinaryFunc::SubTime => Ok(eager!(sub_time)),
            BinaryFunc::SubTimeInterval => Ok(eager!(sub_time_interval)),
            BinaryFunc::SubNumeric => eager!(sub_numeric),
            BinaryFunc::MulInt16 => eager!(mul_int16),
            BinaryFunc::MulInt32 => eager!(mul_int32),
            BinaryFunc::MulInt64 => eager!(mul_int64),
            BinaryFunc::MulFloat32 => Ok(eager!(mul_float32)),
            BinaryFunc::MulFloat64 => Ok(eager!(mul_float64)),
            BinaryFunc::MulNumeric => eager!(mul_numeric),
            BinaryFunc::MulInterval => eager!(mul_interval),
            BinaryFunc::DivInt16 => eager!(div_int16),
            BinaryFunc::DivInt32 => eager!(div_int32),
            BinaryFunc::DivInt64 => eager!(div_int64),
            BinaryFunc::DivFloat32 => eager!(div_float32),
            BinaryFunc::DivFloat64 => eager!(div_float64),
            BinaryFunc::DivNumeric => eager!(div_numeric),
            BinaryFunc::DivInterval => eager!(div_interval),
            BinaryFunc::ModInt16 => eager!(mod_int16),
            BinaryFunc::ModInt32 => eager!(mod_int32),
            BinaryFunc::ModInt64 => eager!(mod_int64),
            BinaryFunc::ModFloat32 => eager!(mod_float32),
            BinaryFunc::ModFloat64 => eager!(mod_float64),
            BinaryFunc::ModNumeric => eager!(mod_numeric),
            BinaryFunc::Eq => Ok(eager!(eq)),
            BinaryFunc::NotEq => Ok(eager!(not_eq)),
            BinaryFunc::Lt => Ok(eager!(lt)),
            BinaryFunc::Lte => Ok(eager!(lte)),
            BinaryFunc::Gt => Ok(eager!(gt)),
            BinaryFunc::Gte => Ok(eager!(gte)),
            BinaryFunc::IsLikePatternMatch { case_insensitive } => {
                eager!(is_like_pattern_match_dynamic, *case_insensitive)
            }
            BinaryFunc::IsRegexpMatch { case_insensitive } => {
                eager!(is_regexp_match_dynamic, *case_insensitive)
            }
            BinaryFunc::ToCharTimestamp => Ok(eager!(to_char_timestamp, temp_storage)),
            BinaryFunc::ToCharTimestampTz => Ok(eager!(to_char_timestamptz, temp_storage)),
            BinaryFunc::DatePartInterval => {
                eager!(|a, b: Datum| date_part_interval(a, b.unwrap_interval()))
            }
            BinaryFunc::DatePartTimestamp => {
                eager!(|a, b: Datum| date_part_timestamp(a, b.unwrap_timestamp()))
            }
            BinaryFunc::DatePartTimestampTz => {
                eager!(|a, b: Datum| date_part_timestamp(a, b.unwrap_timestamptz()))
            }
            BinaryFunc::DateTruncTimestamp => {
                eager!(|a, b: Datum| date_trunc(a, b.unwrap_timestamp()))
            }
            BinaryFunc::DateTruncTimestampTz => {
                eager!(|a, b: Datum| date_trunc(a, b.unwrap_timestamptz()))
            }
            BinaryFunc::TimezoneTimestamp => {
                eager!(|a: Datum, b: Datum| parse_timezone(a.unwrap_str())
                    .and_then(|tz| timezone_timestamp(tz, b.unwrap_timestamp())))
            }
            BinaryFunc::TimezoneTimestampTz => {
                eager!(|a: Datum, b: Datum| parse_timezone(a.unwrap_str())
                    .map(|tz| timezone_timestamptz(tz, b.unwrap_timestamptz())))
            }
            BinaryFunc::TimezoneTime { wall_time } => {
                eager!(
                    |a: Datum, b: Datum| parse_timezone(a.unwrap_str()).map(|tz| timezone_time(
                        tz,
                        b.unwrap_time(),
                        wall_time
                    ))
                )
            }
            BinaryFunc::TimezoneIntervalTimestamp => eager!(timezone_interval_timestamp),
            BinaryFunc::TimezoneIntervalTimestampTz => eager!(timezone_interval_timestamptz),
            BinaryFunc::TimezoneIntervalTime => eager!(timezone_interval_time),
            BinaryFunc::TextConcat => Ok(eager!(text_concat_binary, temp_storage)),
            BinaryFunc::JsonbGetInt64 { stringify } => {
                Ok(eager!(jsonb_get_int64, temp_storage, *stringify))
            }
            BinaryFunc::JsonbGetString { stringify } => {
                Ok(eager!(jsonb_get_string, temp_storage, *stringify))
            }
            BinaryFunc::JsonbGetPath { stringify } => {
                Ok(eager!(jsonb_get_path, temp_storage, *stringify))
            }
            BinaryFunc::JsonbContainsString => Ok(eager!(jsonb_contains_string)),
            BinaryFunc::JsonbConcat => Ok(eager!(jsonb_concat, temp_storage)),
            BinaryFunc::JsonbContainsJsonb => Ok(eager!(jsonb_contains_jsonb)),
            BinaryFunc::JsonbDeleteInt64 => Ok(eager!(jsonb_delete_int64, temp_storage)),
            BinaryFunc::JsonbDeleteString => Ok(eager!(jsonb_delete_string, temp_storage)),
            BinaryFunc::MapContainsKey => Ok(eager!(map_contains_key)),
            BinaryFunc::MapGetValue => Ok(eager!(map_get_value)),
            BinaryFunc::MapGetValues => Ok(eager!(map_get_values, temp_storage)),
            BinaryFunc::MapContainsAllKeys => Ok(eager!(map_contains_all_keys)),
            BinaryFunc::MapContainsAnyKeys => Ok(eager!(map_contains_any_keys)),
            BinaryFunc::MapContainsMap => Ok(eager!(map_contains_map)),
            BinaryFunc::RoundNumeric => eager!(round_numeric_binary),
            BinaryFunc::ConvertFrom => eager!(convert_from),
            BinaryFunc::Encode => eager!(encode, temp_storage),
            BinaryFunc::Decode => eager!(decode, temp_storage),
            BinaryFunc::Left => eager!(left),
            BinaryFunc::Position => eager!(position),
            BinaryFunc::Right => eager!(right),
            BinaryFunc::Trim => Ok(eager!(trim)),
            BinaryFunc::TrimLeading => Ok(eager!(trim_leading)),
            BinaryFunc::TrimTrailing => Ok(eager!(trim_trailing)),
            BinaryFunc::EncodedBytesCharLength => eager!(encoded_bytes_char_length),
            BinaryFunc::ListIndex => Ok(eager!(list_index)),
            BinaryFunc::ListLengthMax { max_dim } => eager!(list_length_max, *max_dim),
            BinaryFunc::ArrayLength => Ok(eager!(array_length)),
            BinaryFunc::ArrayContains => Ok(eager!(array_contains)),
            BinaryFunc::ArrayIndex => Ok(eager!(array_index)),
            BinaryFunc::ArrayLower => Ok(eager!(array_lower)),
            BinaryFunc::ArrayUpper => Ok(eager!(array_upper)),
            BinaryFunc::ListListConcat => Ok(eager!(list_list_concat, temp_storage)),
            BinaryFunc::ListElementConcat => Ok(eager!(list_element_concat, temp_storage)),
            BinaryFunc::ElementListConcat => Ok(eager!(element_list_concat, temp_storage)),
            BinaryFunc::DigestString => eager!(digest_string, temp_storage),
            BinaryFunc::DigestBytes => eager!(digest_bytes, temp_storage),
            BinaryFunc::MzRenderTypemod => Ok(eager!(mz_render_typemod, temp_storage)),
            BinaryFunc::LogNumeric => eager!(log_base_numeric),
            BinaryFunc::Power => eager!(power),
            BinaryFunc::PowerNumeric => eager!(power_numeric),
            BinaryFunc::RepeatString => eager!(repeat_string, temp_storage),
        }
    }

    pub fn output_type(&self, input1_type: ColumnType, input2_type: ColumnType) -> ColumnType {
        use BinaryFunc::*;
        let in_nullable = input1_type.nullable || input2_type.nullable;
        let is_div_mod = matches!(
            self,
            DivInt16
                | ModInt16
                | DivInt32
                | ModInt32
                | DivInt64
                | ModInt64
                | DivFloat32
                | ModFloat32
                | DivFloat64
                | ModFloat64
                | DivNumeric
                | ModNumeric
        );
        match self {
            And | Or | Eq | NotEq | Lt | Lte | Gt | Gte | ArrayContains => {
                ScalarType::Bool.nullable(in_nullable)
            }

            IsLikePatternMatch { .. } | IsRegexpMatch { .. } => {
                // The output can be null if the pattern is invalid.
                ScalarType::Bool.nullable(true)
            }

            ToCharTimestamp | ToCharTimestampTz | ConvertFrom | Left | Right | Trim
            | TrimLeading | TrimTrailing => ScalarType::String.nullable(in_nullable),

            AddInt16 | SubInt16 | MulInt16 | DivInt16 | ModInt16 | BitAndInt16 | BitOrInt16
            | BitXorInt16 | BitShiftLeftInt16 | BitShiftRightInt16 => {
                ScalarType::Int16.nullable(in_nullable || is_div_mod)
            }

            AddInt32
            | SubInt32
            | MulInt32
            | DivInt32
            | ModInt32
            | BitAndInt32
            | BitOrInt32
            | BitXorInt32
            | BitShiftLeftInt32
            | BitShiftRightInt32
            | EncodedBytesCharLength
            | SubDate => ScalarType::Int32.nullable(in_nullable || is_div_mod),

            AddInt64 | SubInt64 | MulInt64 | DivInt64 | ModInt64 | BitAndInt64 | BitOrInt64
            | BitXorInt64 | BitShiftLeftInt64 | BitShiftRightInt64 => {
                ScalarType::Int64.nullable(in_nullable || is_div_mod)
            }

            AddFloat32 | SubFloat32 | MulFloat32 | DivFloat32 | ModFloat32 => {
                ScalarType::Float32.nullable(in_nullable || is_div_mod)
            }

            AddFloat64 | SubFloat64 | MulFloat64 | DivFloat64 | ModFloat64 => {
                ScalarType::Float64.nullable(in_nullable || is_div_mod)
            }

            AddInterval | SubInterval | SubTimestamp | SubTimestampTz | MulInterval
            | DivInterval => ScalarType::Interval.nullable(in_nullable),

            AddTimestampInterval
            | SubTimestampInterval
            | AddTimestampTzInterval
            | SubTimestampTzInterval
            | AddTimeInterval
            | SubTimeInterval => input1_type,

            AddDateInterval | SubDateInterval | AddDateTime | DateTruncTimestamp => {
                ScalarType::Timestamp.nullable(true)
            }

            TimezoneTimestampTz | TimezoneIntervalTimestampTz => {
                ScalarType::Timestamp.nullable(in_nullable)
            }

            DatePartInterval | DatePartTimestamp | DatePartTimestampTz => {
                ScalarType::Float64.nullable(true)
            }

            DateTruncTimestampTz => ScalarType::TimestampTz.nullable(true),

            TimezoneTimestamp | TimezoneIntervalTimestamp => {
                ScalarType::TimestampTz.nullable(in_nullable)
            }

            TimezoneTime { .. } | TimezoneIntervalTime => ScalarType::Time.nullable(in_nullable),

            SubTime => ScalarType::Interval.nullable(true),

            MzRenderTypemod | TextConcat => ScalarType::String.nullable(in_nullable),

            JsonbGetInt64 { stringify: true }
            | JsonbGetString { stringify: true }
            | JsonbGetPath { stringify: true } => ScalarType::String.nullable(true),

            JsonbGetInt64 { stringify: false }
            | JsonbGetString { stringify: false }
            | JsonbGetPath { stringify: false }
            | JsonbConcat
            | JsonbDeleteInt64
            | JsonbDeleteString => ScalarType::Jsonb.nullable(true),

            JsonbContainsString | JsonbContainsJsonb | MapContainsKey | MapContainsAllKeys
            | MapContainsAnyKeys | MapContainsMap => ScalarType::Bool.nullable(in_nullable),

            MapGetValue => input1_type
                .scalar_type
                .unwrap_map_value_type()
                .clone()
                .nullable(true),

            MapGetValues => ScalarType::Array(Box::new(
                input1_type.scalar_type.unwrap_map_value_type().clone(),
            ))
            .nullable(true),

            ListIndex => input1_type
                .scalar_type
                .unwrap_list_element_type()
                .clone()
                .nullable(true),

            ArrayIndex => input1_type
                .scalar_type
                .unwrap_array_element_type()
                .clone()
                .nullable(true),

            ListLengthMax { .. } | ArrayLength | ArrayLower | ArrayUpper => {
                ScalarType::Int64.nullable(true)
            }

            ListListConcat | ListElementConcat => input1_type
                .scalar_type
                .default_embedded_value()
                .nullable(true),

            ElementListConcat => input2_type
                .scalar_type
                .default_embedded_value()
                .nullable(true),

            DigestString | DigestBytes => ScalarType::Bytes.nullable(true),
            Position => ScalarType::Int32.nullable(in_nullable),
            Encode => ScalarType::String.nullable(in_nullable),
            Decode => ScalarType::Bytes.nullable(in_nullable),
            Power => ScalarType::Float64.nullable(in_nullable),
            RepeatString => input1_type.scalar_type.nullable(in_nullable),

            AddNumeric | DivNumeric | LogNumeric | ModNumeric | MulNumeric | PowerNumeric
            | RoundNumeric | SubNumeric => {
                ScalarType::Numeric { scale: None }.nullable(in_nullable)
            }
        }
    }

    /// Whether the function output is NULL if any of its inputs are NULL.
    pub fn propagates_nulls(&self) -> bool {
        !matches!(
            self,
            BinaryFunc::And
                | BinaryFunc::Or
                | BinaryFunc::ListListConcat
                | BinaryFunc::ListElementConcat
                | BinaryFunc::ElementListConcat
        )
    }

    /// Whether the function might return NULL even if none of its inputs are
    /// NULL.
    ///
    /// This is presently conservative, and may indicate that a function
    /// introduces nulls even when it does not.
    pub fn introduces_nulls(&self) -> bool {
        use BinaryFunc::*;
        !matches!(
            self,
            And | Or
                | Eq
                | NotEq
                | Lt
                | Lte
                | Gt
                | Gte
                | AddInt16
                | AddInt32
                | AddInt64
                | AddFloat32
                | AddFloat64
                | AddTimestampInterval
                | AddTimestampTzInterval
                | AddDateTime
                | AddDateInterval
                | AddTimeInterval
                | AddInterval
                | BitAndInt16
                | BitAndInt32
                | BitAndInt64
                | BitOrInt16
                | BitOrInt32
                | BitOrInt64
                | BitXorInt16
                | BitXorInt32
                | BitXorInt64
                | BitShiftLeftInt16
                | BitShiftLeftInt32
                | BitShiftLeftInt64
                | BitShiftRightInt16
                | BitShiftRightInt32
                | BitShiftRightInt64
                | SubInterval
                | MulInterval
                | DivInterval
                | AddNumeric
                | SubInt16
                | SubInt32
                | SubInt64
                | SubFloat32
                | SubFloat64
                | SubTimestamp
                | SubTimestampTz
                | SubTimestampInterval
                | SubTimestampTzInterval
                | SubDate
                | SubDateInterval
                | SubTime
                | SubTimeInterval
                | SubNumeric
                | MulInt16
                | MulInt32
                | MulInt64
                | MulFloat32
                | MulFloat64
                | MulNumeric
                | DivInt16
                | DivInt32
                | DivInt64
                | DivFloat32
                | DivFloat64
                | ModInt16
                | ModInt32
                | ModInt64
                | ModFloat32
                | ModFloat64
                | ModNumeric
        )
    }

    pub fn is_infix_op(&self) -> bool {
        use BinaryFunc::*;
        match self {
            And
            | Or
            | AddInt16
            | AddInt32
            | AddInt64
            | AddFloat32
            | AddFloat64
            | AddTimestampInterval
            | AddTimestampTzInterval
            | AddDateTime
            | AddDateInterval
            | AddTimeInterval
            | AddInterval
            | BitAndInt16
            | BitAndInt32
            | BitAndInt64
            | BitOrInt16
            | BitOrInt32
            | BitOrInt64
            | BitXorInt16
            | BitXorInt32
            | BitXorInt64
            | BitShiftLeftInt16
            | BitShiftLeftInt32
            | BitShiftLeftInt64
            | BitShiftRightInt16
            | BitShiftRightInt32
            | BitShiftRightInt64
            | SubInterval
            | MulInterval
            | DivInterval
            | AddNumeric
            | SubInt16
            | SubInt32
            | SubInt64
            | SubFloat32
            | SubFloat64
            | SubTimestamp
            | SubTimestampTz
            | SubTimestampInterval
            | SubTimestampTzInterval
            | SubDate
            | SubDateInterval
            | SubTime
            | SubTimeInterval
            | SubNumeric
            | MulInt16
            | MulInt32
            | MulInt64
            | MulFloat32
            | MulFloat64
            | MulNumeric
            | DivInt16
            | DivInt32
            | DivInt64
            | DivFloat32
            | DivFloat64
            | DivNumeric
            | ModInt16
            | ModInt32
            | ModInt64
            | ModFloat32
            | ModFloat64
            | ModNumeric
            | Eq
            | NotEq
            | Lt
            | Lte
            | Gt
            | Gte
            | JsonbConcat
            | JsonbContainsJsonb
            | JsonbGetInt64 { .. }
            | JsonbGetString { .. }
            | JsonbGetPath { .. }
            | JsonbContainsString
            | JsonbDeleteInt64
            | JsonbDeleteString
            | MapContainsKey
            | MapGetValue
            | MapGetValues
            | MapContainsAllKeys
            | MapContainsAnyKeys
            | MapContainsMap
            | TextConcat
            | ListIndex
            | IsRegexpMatch { .. }
            | ArrayContains
            | ArrayIndex
            | ArrayLength
            | ArrayLower
            | ArrayUpper
            | ListListConcat
            | ListElementConcat
            | ElementListConcat => true,
            IsLikePatternMatch { .. }
            | ToCharTimestamp
            | ToCharTimestampTz
            | DatePartInterval
            | DatePartTimestamp
            | DatePartTimestampTz
            | DateTruncTimestamp
            | DateTruncTimestampTz
            | TimezoneTimestamp
            | TimezoneTimestampTz
            | TimezoneTime { .. }
            | TimezoneIntervalTimestamp
            | TimezoneIntervalTimestampTz
            | TimezoneIntervalTime
            | RoundNumeric
            | ConvertFrom
            | Left
            | Position
            | Right
            | Trim
            | TrimLeading
            | TrimTrailing
            | EncodedBytesCharLength
            | ListLengthMax { .. }
            | DigestString
            | DigestBytes
            | MzRenderTypemod
            | Encode
            | Decode
            | LogNumeric
            | Power
            | PowerNumeric
            | RepeatString => false,
        }
    }

    /// Returns the negation of the given binary function, if it exists.
    pub fn negate(&self) -> Option<Self> {
        match self {
            BinaryFunc::Eq => Some(BinaryFunc::NotEq),
            BinaryFunc::NotEq => Some(BinaryFunc::Eq),
            BinaryFunc::Lt => Some(BinaryFunc::Gte),
            BinaryFunc::Gte => Some(BinaryFunc::Lt),
            BinaryFunc::Gt => Some(BinaryFunc::Lte),
            BinaryFunc::Lte => Some(BinaryFunc::Gt),
            _ => None,
        }
    }
}

impl fmt::Display for BinaryFunc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            BinaryFunc::And => f.write_str("&&"),
            BinaryFunc::Or => f.write_str("||"),
            BinaryFunc::AddInt16 => f.write_str("+"),
            BinaryFunc::AddInt32 => f.write_str("+"),
            BinaryFunc::AddInt64 => f.write_str("+"),
            BinaryFunc::AddFloat32 => f.write_str("+"),
            BinaryFunc::AddFloat64 => f.write_str("+"),
            BinaryFunc::AddNumeric => f.write_str("+"),
            BinaryFunc::AddInterval => f.write_str("+"),
            BinaryFunc::AddTimestampInterval => f.write_str("+"),
            BinaryFunc::AddTimestampTzInterval => f.write_str("+"),
            BinaryFunc::AddDateTime => f.write_str("+"),
            BinaryFunc::AddDateInterval => f.write_str("+"),
            BinaryFunc::AddTimeInterval => f.write_str("+"),
            BinaryFunc::BitAndInt16 => f.write_str("&"),
            BinaryFunc::BitAndInt32 => f.write_str("&"),
            BinaryFunc::BitAndInt64 => f.write_str("&"),
            BinaryFunc::BitOrInt16 => f.write_str("|"),
            BinaryFunc::BitOrInt32 => f.write_str("|"),
            BinaryFunc::BitOrInt64 => f.write_str("|"),
            BinaryFunc::BitXorInt16 => f.write_str("#"),
            BinaryFunc::BitXorInt32 => f.write_str("#"),
            BinaryFunc::BitXorInt64 => f.write_str("#"),
            BinaryFunc::BitShiftLeftInt16 => f.write_str("<<"),
            BinaryFunc::BitShiftLeftInt32 => f.write_str("<<"),
            BinaryFunc::BitShiftLeftInt64 => f.write_str("<<"),
            BinaryFunc::BitShiftRightInt16 => f.write_str(">>"),
            BinaryFunc::BitShiftRightInt32 => f.write_str(">>"),
            BinaryFunc::BitShiftRightInt64 => f.write_str(">>"),
            BinaryFunc::SubInt16 => f.write_str("-"),
            BinaryFunc::SubInt32 => f.write_str("-"),
            BinaryFunc::SubInt64 => f.write_str("-"),
            BinaryFunc::SubFloat32 => f.write_str("-"),
            BinaryFunc::SubFloat64 => f.write_str("-"),
            BinaryFunc::SubNumeric => f.write_str("-"),
            BinaryFunc::SubInterval => f.write_str("-"),
            BinaryFunc::SubTimestamp => f.write_str("-"),
            BinaryFunc::SubTimestampTz => f.write_str("-"),
            BinaryFunc::SubTimestampInterval => f.write_str("-"),
            BinaryFunc::SubTimestampTzInterval => f.write_str("-"),
            BinaryFunc::SubDate => f.write_str("-"),
            BinaryFunc::SubDateInterval => f.write_str("-"),
            BinaryFunc::SubTime => f.write_str("-"),
            BinaryFunc::SubTimeInterval => f.write_str("-"),
            BinaryFunc::MulInt16 => f.write_str("*"),
            BinaryFunc::MulInt32 => f.write_str("*"),
            BinaryFunc::MulInt64 => f.write_str("*"),
            BinaryFunc::MulFloat32 => f.write_str("*"),
            BinaryFunc::MulFloat64 => f.write_str("*"),
            BinaryFunc::MulNumeric => f.write_str("*"),
            BinaryFunc::MulInterval => f.write_str("*"),
            BinaryFunc::DivInt16 => f.write_str("/"),
            BinaryFunc::DivInt32 => f.write_str("/"),
            BinaryFunc::DivInt64 => f.write_str("/"),
            BinaryFunc::DivFloat32 => f.write_str("/"),
            BinaryFunc::DivFloat64 => f.write_str("/"),
            BinaryFunc::DivNumeric => f.write_str("/"),
            BinaryFunc::DivInterval => f.write_str("/"),
            BinaryFunc::ModInt16 => f.write_str("%"),
            BinaryFunc::ModInt32 => f.write_str("%"),
            BinaryFunc::ModInt64 => f.write_str("%"),
            BinaryFunc::ModFloat32 => f.write_str("%"),
            BinaryFunc::ModFloat64 => f.write_str("%"),
            BinaryFunc::ModNumeric => f.write_str("%"),
            BinaryFunc::Eq => f.write_str("="),
            BinaryFunc::NotEq => f.write_str("!="),
            BinaryFunc::Lt => f.write_str("<"),
            BinaryFunc::Lte => f.write_str("<="),
            BinaryFunc::Gt => f.write_str(">"),
            BinaryFunc::Gte => f.write_str(">="),
            BinaryFunc::IsLikePatternMatch {
                case_insensitive: false,
            } => f.write_str("like"),
            BinaryFunc::IsLikePatternMatch {
                case_insensitive: true,
            } => f.write_str("ilike"),
            BinaryFunc::IsRegexpMatch {
                case_insensitive: false,
            } => f.write_str("~"),
            BinaryFunc::IsRegexpMatch {
                case_insensitive: true,
            } => f.write_str("~*"),
            BinaryFunc::ToCharTimestamp => f.write_str("tocharts"),
            BinaryFunc::ToCharTimestampTz => f.write_str("tochartstz"),
            BinaryFunc::DatePartInterval => f.write_str("date_partiv"),
            BinaryFunc::DatePartTimestamp => f.write_str("date_partts"),
            BinaryFunc::DatePartTimestampTz => f.write_str("date_parttstz"),
            BinaryFunc::DateTruncTimestamp => f.write_str("date_truncts"),
            BinaryFunc::DateTruncTimestampTz => f.write_str("date_trunctstz"),
            BinaryFunc::TimezoneTimestamp => f.write_str("timezonets"),
            BinaryFunc::TimezoneTimestampTz => f.write_str("timezonetstz"),
            BinaryFunc::TimezoneTime { .. } => f.write_str("timezonet"),
            BinaryFunc::TimezoneIntervalTimestamp => f.write_str("timezoneits"),
            BinaryFunc::TimezoneIntervalTimestampTz => f.write_str("timezoneitstz"),
            BinaryFunc::TimezoneIntervalTime => f.write_str("timezoneit"),
            BinaryFunc::TextConcat => f.write_str("||"),
            BinaryFunc::JsonbGetInt64 { stringify: false } => f.write_str("->"),
            BinaryFunc::JsonbGetInt64 { stringify: true } => f.write_str("->>"),
            BinaryFunc::JsonbGetString { stringify: false } => f.write_str("->"),
            BinaryFunc::JsonbGetString { stringify: true } => f.write_str("->>"),
            BinaryFunc::JsonbGetPath { stringify: false } => f.write_str("#>"),
            BinaryFunc::JsonbGetPath { stringify: true } => f.write_str("#>>"),
            BinaryFunc::JsonbContainsString | BinaryFunc::MapContainsKey => f.write_str("?"),
            BinaryFunc::JsonbConcat => f.write_str("||"),
            BinaryFunc::JsonbContainsJsonb | BinaryFunc::MapContainsMap => f.write_str("@>"),
            BinaryFunc::JsonbDeleteInt64 => f.write_str("-"),
            BinaryFunc::JsonbDeleteString => f.write_str("-"),
            BinaryFunc::MapGetValue | BinaryFunc::MapGetValues => f.write_str("->"),
            BinaryFunc::MapContainsAllKeys => f.write_str("?&"),
            BinaryFunc::MapContainsAnyKeys => f.write_str("?|"),
            BinaryFunc::RoundNumeric => f.write_str("round"),
            BinaryFunc::ConvertFrom => f.write_str("convert_from"),
            BinaryFunc::Left => f.write_str("left"),
            BinaryFunc::Position => f.write_str("position"),
            BinaryFunc::Right => f.write_str("right"),
            BinaryFunc::Trim => f.write_str("btrim"),
            BinaryFunc::TrimLeading => f.write_str("ltrim"),
            BinaryFunc::TrimTrailing => f.write_str("rtrim"),
            BinaryFunc::EncodedBytesCharLength => f.write_str("length"),
            BinaryFunc::ListIndex => f.write_str("list_index"),
            BinaryFunc::ListLengthMax { .. } => f.write_str("list_length_max"),
            BinaryFunc::ArrayContains => f.write_str("array_contains"),
            BinaryFunc::ArrayIndex => f.write_str("array_index"),
            BinaryFunc::ArrayLength => f.write_str("array_length"),
            BinaryFunc::ArrayLower => f.write_str("array_lower"),
            BinaryFunc::ArrayUpper => f.write_str("array_upper"),
            BinaryFunc::ListListConcat => f.write_str("||"),
            BinaryFunc::ListElementConcat => f.write_str("||"),
            BinaryFunc::ElementListConcat => f.write_str("||"),
            BinaryFunc::DigestString | BinaryFunc::DigestBytes => f.write_str("digest"),
            BinaryFunc::MzRenderTypemod => f.write_str("mz_render_typemod"),
            BinaryFunc::Encode => f.write_str("encode"),
            BinaryFunc::Decode => f.write_str("decode"),
            BinaryFunc::LogNumeric => f.write_str("log"),
            BinaryFunc::Power => f.write_str("power"),
            BinaryFunc::PowerNumeric => f.write_str("power_numeric"),
            BinaryFunc::RepeatString => f.write_str("repeat"),
        }
    }
}

// This trait will eventualy be annotated with #[enum_dispatch] to autogenerate the UnaryFunc enum
trait UnaryFuncTrait {
    fn eval<'a>(
        &'a self,
        datums: &[Datum<'a>],
        temp_storage: &'a RowArena,
        a: &'a MirScalarExpr,
    ) -> Result<Datum<'a>, EvalError>;
    fn output_type(&self, input_type: ColumnType) -> ColumnType;
    fn propagates_nulls(&self) -> bool;
    fn introduces_nulls(&self) -> bool;
    fn preserves_uniqueness(&self) -> bool;
}

#[derive(
    Ord, PartialOrd, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash, MzEnumReflect,
)]
pub enum UnaryFunc {
    Not(Not),
    IsNull(IsNull),
    IsTrue(IsTrue),
    IsFalse(IsFalse),
    BitNotInt16(BitNotInt16),
    BitNotInt32(BitNotInt32),
    BitNotInt64(BitNotInt64),
    NegInt16(NegInt16),
    NegInt32(NegInt32),
    NegInt64(NegInt64),
    NegFloat32(NegFloat32),
    NegFloat64(NegFloat64),
    NegNumeric,
    NegInterval,
    SqrtFloat64(SqrtFloat64),
    SqrtNumeric,
    CbrtFloat64(CbrtFloat64),
    AbsInt16(AbsInt16),
    AbsInt32(AbsInt32),
    AbsInt64(AbsInt64),
    AbsFloat32(AbsFloat32),
    AbsFloat64(AbsFloat64),
    AbsNumeric,
    CastBoolToString,
    CastBoolToStringNonstandard,
    CastBoolToInt32,
    CastInt16ToFloat32,
    CastInt16ToFloat64,
    CastInt16ToInt32,
    CastInt16ToInt64,
    CastInt16ToString,
    CastInt32ToBool,
    CastInt32ToFloat32,
    CastInt32ToFloat64,
    CastInt32ToOid,
    CastInt32ToRegProc,
    CastInt32ToInt16,
    CastInt32ToInt64,
    CastInt32ToString,
    CastOidToInt32,
    CastOidToRegProc,
    CastRegProcToOid,
    CastInt64ToInt16,
    CastInt64ToInt32,
    CastInt16ToNumeric(Option<u8>),
    CastInt32ToNumeric(Option<u8>),
    CastInt64ToBool,
    CastInt64ToNumeric(Option<u8>),
    CastInt64ToFloat32,
    CastInt64ToFloat64,
    CastInt64ToString,
    CastFloat32ToInt16(CastFloat32ToInt16),
    CastFloat32ToInt32(CastFloat32ToInt32),
    CastFloat32ToInt64(CastFloat32ToInt64),
    CastFloat32ToFloat64(CastFloat32ToFloat64),
    CastFloat32ToString(CastFloat32ToString),
    CastFloat32ToNumeric(Option<u8>),
    CastFloat64ToNumeric(Option<u8>),
    CastFloat64ToInt16(CastFloat64ToInt16),
    CastFloat64ToInt32(CastFloat64ToInt32),
    CastFloat64ToInt64(CastFloat64ToInt64),
    CastFloat64ToFloat32(CastFloat64ToFloat32),
    CastFloat64ToString(CastFloat64ToString),
    CastNumericToFloat32,
    CastNumericToFloat64,
    CastNumericToInt16,
    CastNumericToInt32,
    CastNumericToInt64,
    CastNumericToString,
    CastStringToBool,
    CastStringToBytes,
    CastStringToInt16,
    CastStringToInt32,
    CastStringToInt64,
    CastStringToFloat32,
    CastStringToFloat64,
    CastStringToDate,
    CastStringToArray {
        // Target array's type.
        return_ty: ScalarType,
        // The expression to cast the discovered array elements to the array's
        // element type.
        cast_expr: Box<MirScalarExpr>,
    },
    CastStringToList {
        // Target list's type
        return_ty: ScalarType,
        // The expression to cast the discovered list elements to the list's
        // element type.
        cast_expr: Box<MirScalarExpr>,
    },
    CastStringToMap {
        // Target map's value type
        return_ty: ScalarType,
        // The expression used to cast the discovered values to the map's value
        // type.
        cast_expr: Box<MirScalarExpr>,
    },
    CastStringToTime,
    CastStringToTimestamp,
    CastStringToTimestampTz,
    CastStringToInterval,
    CastStringToNumeric(Option<u8>),
    CastStringToUuid,
    CastStringToChar {
        length: Option<usize>,
        fail_on_len: bool,
    },
    /// All Char data is stored in Datum::String with its blank padding removed
    /// (i.e. trimmed), so this function provides a means of restoring any
    /// removed padding.
    PadChar {
        length: Option<usize>,
    },
    CastStringToVarChar {
        length: Option<usize>,
        fail_on_len: bool,
    },
    CastCharToString,
    CastVarCharToString,
    CastDateToTimestamp,
    CastDateToTimestampTz,
    CastDateToString,
    CastTimeToInterval,
    CastTimeToString,
    CastTimestampToDate,
    CastTimestampToTimestampTz,
    CastTimestampToString,
    CastTimestampTzToDate,
    CastTimestampTzToTimestamp,
    CastTimestampTzToString,
    CastIntervalToString,
    CastIntervalToTime,
    CastBytesToString,
    CastStringToJsonb,
    CastJsonbToString,
    CastJsonbOrNullToJsonb,
    CastJsonbToInt16,
    CastJsonbToInt32,
    CastJsonbToInt64,
    CastJsonbToFloat32,
    CastJsonbToFloat64,
    CastJsonbToNumeric(Option<u8>),
    CastJsonbToBool,
    CastUuidToString,
    CastRecordToString {
        ty: ScalarType,
    },
    CastArrayToString {
        ty: ScalarType,
    },
    CastListToString {
        ty: ScalarType,
    },
    CastList1ToList2 {
        // List2's type
        return_ty: ScalarType,
        // The expression to cast List1's elements to List2's elements' type
        cast_expr: Box<MirScalarExpr>,
    },
    CastMapToString {
        ty: ScalarType,
    },
    CastInPlace {
        return_ty: ScalarType,
    },
    CeilFloat32(CeilFloat32),
    CeilFloat64(CeilFloat64),
    CeilNumeric,
    FloorFloat32(FloorFloat32),
    FloorFloat64(FloorFloat64),
    FloorNumeric,
    Ascii,
    BitLengthBytes,
    BitLengthString,
    ByteLengthBytes,
    ByteLengthString,
    CharLength,
    IsRegexpMatch(Regex),
    RegexpMatch(Regex),
    DatePartInterval(DateTimeUnits),
    DatePartTimestamp(DateTimeUnits),
    DatePartTimestampTz(DateTimeUnits),
    DateTruncTimestamp(DateTimeUnits),
    DateTruncTimestampTz(DateTimeUnits),
    TimezoneTimestamp(Timezone),
    TimezoneTimestampTz(Timezone),
    TimezoneTime {
        tz: Timezone,
        wall_time: NaiveDateTime,
    },
    ToTimestamp(ToTimestamp),
    JsonbArrayLength,
    JsonbTypeof,
    JsonbStripNulls,
    JsonbPretty,
    RoundFloat32(RoundFloat32),
    RoundFloat64(RoundFloat64),
    RoundNumeric,
    TrimWhitespace,
    TrimLeadingWhitespace,
    TrimTrailingWhitespace,
    RecordGet(usize),
    ListLength,
    Upper,
    Lower,
    Cos(Cos),
    Cosh(Cosh),
    Sin(Sin),
    Sinh(Sinh),
    Tan(Tan),
    Tanh(Tanh),
    Cot(Cot),
    Log10(Log10),
    Log10Numeric,
    Ln(Ln),
    LnNumeric,
    Exp(Exp),
    ExpNumeric,
    Sleep(Sleep),
    RescaleNumeric(u8),
    PgColumnSize(PgColumnSize),
    MzRowSize(MzRowSize),
}

derive_unary!(
    Not,
    NegFloat32,
    NegFloat64,
    NegInt16,
    NegInt32,
    NegInt64,
    AbsFloat32,
    AbsFloat64,
    AbsInt16,
    AbsInt32,
    AbsInt64,
    BitNotInt16,
    BitNotInt32,
    BitNotInt64,
    RoundFloat32,
    RoundFloat64,
    CeilFloat32,
    CeilFloat64,
    FloorFloat32,
    FloorFloat64,
    CastFloat32ToInt16,
    CastFloat32ToInt32,
    CastFloat32ToInt64,
    CastFloat64ToInt16,
    CastFloat64ToInt32,
    CastFloat64ToInt64,
    CastFloat32ToFloat64,
    CastFloat64ToFloat32,
    CastFloat32ToString,
    PgColumnSize,
    MzRowSize,
    IsNull,
    IsTrue,
    IsFalse,
    Sleep,
    ToTimestamp,
    CastFloat64ToString,
    Cos,
    Cosh,
    Sin,
    Sinh,
    Tan,
    Tanh,
    Cot,
    Log10,
    Ln,
    Exp,
    SqrtFloat64,
    CbrtFloat64
);

impl UnaryFunc {
    pub fn eval_manual<'a>(
        &'a self,
        datums: &[Datum<'a>],
        temp_storage: &'a RowArena,
        a: &'a MirScalarExpr,
    ) -> Result<Datum<'a>, EvalError> {
        let a = a.eval(datums, temp_storage)?;
        if self.propagates_nulls() && a.is_null() {
            return Ok(Datum::Null);
        }

        use UnaryFunc::*;
        match self {
            Not(_)
            | NegFloat32(_)
            | NegFloat64(_)
            | NegInt16(_)
            | NegInt32(_)
            | NegInt64(_)
            | AbsFloat32(_)
            | AbsFloat64(_)
            | AbsInt16(_)
            | AbsInt32(_)
            | AbsInt64(_)
            | BitNotInt16(_)
            | BitNotInt32(_)
            | BitNotInt64(_)
            | RoundFloat32(_)
            | RoundFloat64(_)
            | CeilFloat32(_)
            | CeilFloat64(_)
            | FloorFloat32(_)
            | FloorFloat64(_)
            | CastFloat32ToInt16(_)
            | CastFloat32ToInt32(_)
            | CastFloat32ToInt64(_)
            | CastFloat64ToInt16(_)
            | CastFloat64ToInt32(_)
            | CastFloat64ToInt64(_)
            | CastFloat64ToFloat32(_)
            | PgColumnSize(_)
            | MzRowSize(_)
            | IsNull(_)
            | IsTrue(_)
            | IsFalse(_)
            | CastFloat32ToString(_)
            | Sleep(_)
            | ToTimestamp(_)
            | CastFloat64ToString(_)
            | Cos(_)
            | Cosh(_)
            | Sin(_)
            | Sinh(_)
            | Tan(_)
            | Tanh(_)
            | Cot(_)
            | Log10(_)
            | Ln(_)
            | Exp(_)
            | SqrtFloat64(_)
            | CbrtFloat64(_)
            | CastFloat32ToFloat64(_) => unreachable!(),
            NegNumeric => Ok(neg_numeric(a)),
            NegInterval => Ok(neg_interval(a)),
            AbsNumeric => Ok(abs_numeric(a)),
            CastBoolToString => Ok(cast_bool_to_string(a)),
            CastBoolToStringNonstandard => Ok(cast_bool_to_string_nonstandard(a)),
            CastBoolToInt32 => Ok(cast_bool_to_int32(a)),
            CastFloat32ToNumeric(scale) => cast_float32_to_numeric(a, *scale),
            CastFloat64ToNumeric(scale) => cast_float64_to_numeric(a, *scale),
            CastInt16ToFloat32 => Ok(cast_int16_to_float32(a)),
            CastInt16ToFloat64 => Ok(cast_int16_to_float64(a)),
            CastInt16ToInt32 => Ok(cast_int16_to_int32(a)),
            CastInt16ToInt64 => Ok(cast_int16_to_int64(a)),
            CastInt16ToNumeric(scale) => cast_int16_to_numeric(a, *scale),
            CastInt16ToString => Ok(cast_int16_to_string(a, temp_storage)),
            CastInt32ToBool => Ok(cast_int32_to_bool(a)),
            CastInt32ToFloat32 => Ok(cast_int32_to_float32(a)),
            CastInt32ToFloat64 => Ok(cast_int32_to_float64(a)),
            CastInt32ToInt16 => cast_int32_to_int16(a),
            CastInt32ToInt64 => Ok(cast_int32_to_int64(a)),
            CastInt32ToOid => Ok(a),
            CastInt32ToRegProc => Ok(a),
            CastInt32ToNumeric(scale) => cast_int32_to_numeric(a, *scale),
            CastInt32ToString => Ok(cast_int32_to_string(a, temp_storage)),
            CastOidToInt32 => Ok(a),
            CastRegProcToOid => Ok(a),
            CastOidToRegProc => Ok(a),
            CastInt64ToInt16 => cast_int64_to_int16(a),
            CastInt64ToInt32 => cast_int64_to_int32(a),
            CastInt64ToBool => Ok(cast_int64_to_bool(a)),
            CastInt64ToNumeric(scale) => cast_int64_to_numeric(a, *scale),
            CastInt64ToFloat32 => Ok(cast_int64_to_float32(a)),
            CastInt64ToFloat64 => Ok(cast_int64_to_float64(a)),
            CastInt64ToString => Ok(cast_int64_to_string(a, temp_storage)),
            CastStringToBool => cast_string_to_bool(a),
            CastStringToBytes => cast_string_to_bytes(a, temp_storage),
            CastStringToInt16 => cast_string_to_int16(a),
            CastStringToInt32 => cast_string_to_int32(a),
            CastStringToInt64 => cast_string_to_int64(a),
            CastStringToFloat32 => cast_string_to_float32(a),
            CastStringToFloat64 => cast_string_to_float64(a),
            CastStringToNumeric(scale) => cast_string_to_numeric(a, *scale),
            CastStringToDate => cast_string_to_date(a),
            CastStringToArray { cast_expr, .. } => cast_string_to_array(a, cast_expr, temp_storage),
            CastStringToList {
                cast_expr,
                return_ty,
            } => cast_string_to_list(a, return_ty, cast_expr, temp_storage),
            CastStringToMap {
                cast_expr,
                return_ty,
            } => cast_string_to_map(a, return_ty, cast_expr, temp_storage),
            CastStringToTime => cast_string_to_time(a),
            CastStringToTimestamp => cast_string_to_timestamp(a),
            CastStringToTimestampTz => cast_string_to_timestamptz(a),
            CastStringToInterval => cast_string_to_interval(a),
            CastStringToUuid => cast_string_to_uuid(a),
            CastStringToChar {
                length,
                fail_on_len,
            } => cast_str_to_char(a, *length, *fail_on_len, temp_storage),
            PadChar { length } => pad_char(a, *length, temp_storage),
            CastStringToVarChar {
                length,
                fail_on_len,
            } => cast_string_to_varchar(a, *length, *fail_on_len, temp_storage),
            // This function simply allows the expression of changing a's type from varchar to string
            CastCharToString => Ok(a),
            CastVarCharToString => Ok(a),
            CastStringToJsonb => cast_string_to_jsonb(a, temp_storage),
            CastDateToTimestamp => Ok(cast_date_to_timestamp(a)),
            CastDateToTimestampTz => Ok(cast_date_to_timestamptz(a)),
            CastDateToString => Ok(cast_date_to_string(a, temp_storage)),
            CastNumericToFloat32 => cast_numeric_to_float32(a),
            CastNumericToFloat64 => cast_numeric_to_float64(a),
            CastNumericToInt16 => cast_numeric_to_int16(a),
            CastNumericToInt32 => cast_numeric_to_int32(a),
            CastNumericToInt64 => cast_numeric_to_int64(a),
            CastNumericToString => Ok(cast_numeric_to_string(a, temp_storage)),
            CastTimeToInterval => cast_time_to_interval(a),
            CastTimeToString => Ok(cast_time_to_string(a, temp_storage)),
            CastTimestampToDate => Ok(cast_timestamp_to_date(a)),
            CastTimestampToTimestampTz => Ok(cast_timestamp_to_timestamptz(a)),
            CastTimestampToString => Ok(cast_timestamp_to_string(a, temp_storage)),
            CastTimestampTzToDate => Ok(cast_timestamptz_to_date(a)),
            CastTimestampTzToTimestamp => Ok(cast_timestamptz_to_timestamp(a)),
            CastTimestampTzToString => Ok(cast_timestamptz_to_string(a, temp_storage)),
            CastIntervalToString => Ok(cast_interval_to_string(a, temp_storage)),
            CastIntervalToTime => Ok(cast_interval_to_time(a)),
            CastBytesToString => Ok(cast_bytes_to_string(a, temp_storage)),
            CastJsonbOrNullToJsonb => Ok(cast_jsonb_or_null_to_jsonb(a)),
            CastJsonbToString => Ok(cast_jsonb_to_string(a, temp_storage)),
            CastJsonbToInt16 => cast_jsonb_to_int16(a),
            CastJsonbToInt32 => cast_jsonb_to_int32(a),
            CastJsonbToInt64 => cast_jsonb_to_int64(a),
            CastJsonbToFloat32 => cast_jsonb_to_float32(a),
            CastJsonbToFloat64 => cast_jsonb_to_float64(a),
            CastJsonbToNumeric(scale) => cast_jsonb_to_numeric(a, *scale),
            CastJsonbToBool => cast_jsonb_to_bool(a),
            CastUuidToString => Ok(cast_uuid_to_string(a, temp_storage)),
            CastRecordToString { ty }
            | CastArrayToString { ty }
            | CastListToString { ty }
            | CastMapToString { ty } => Ok(cast_collection_to_string(a, ty, temp_storage)),
            CastList1ToList2 { cast_expr, .. } => cast_list1_to_list2(a, &*cast_expr, temp_storage),
            CastInPlace { .. } => Ok(a),
            CeilNumeric => Ok(ceil_numeric(a)),
            FloorNumeric => Ok(floor_numeric(a)),
            SqrtNumeric => sqrt_numeric(a),
            Ascii => Ok(ascii(a)),
            BitLengthString => bit_length(a.unwrap_str()),
            BitLengthBytes => bit_length(a.unwrap_bytes()),
            ByteLengthString => byte_length(a.unwrap_str()),
            ByteLengthBytes => byte_length(a.unwrap_bytes()),
            CharLength => char_length(a),
            IsRegexpMatch(regex) => Ok(is_regexp_match_static(a, &regex)),
            RegexpMatch(regex) => regexp_match_static(a, temp_storage, &regex),
            DatePartInterval(units) => date_part_interval_inner(*units, a.unwrap_interval()),
            DatePartTimestamp(units) => date_part_timestamp_inner(*units, a.unwrap_timestamp()),
            DatePartTimestampTz(units) => date_part_timestamp_inner(*units, a.unwrap_timestamptz()),
            DateTruncTimestamp(units) => date_trunc_inner(*units, a.unwrap_timestamp()),
            DateTruncTimestampTz(units) => date_trunc_inner(*units, a.unwrap_timestamptz()),
            TimezoneTimestamp(tz) => timezone_timestamp(*tz, a.unwrap_timestamp()),
            TimezoneTimestampTz(tz) => Ok(timezone_timestamptz(*tz, a.unwrap_timestamptz())),
            TimezoneTime { tz, wall_time } => Ok(timezone_time(*tz, a.unwrap_time(), wall_time)),
            JsonbArrayLength => Ok(jsonb_array_length(a)),
            JsonbTypeof => Ok(jsonb_typeof(a)),
            JsonbStripNulls => Ok(jsonb_strip_nulls(a, temp_storage)),
            JsonbPretty => Ok(jsonb_pretty(a, temp_storage)),
            RoundNumeric => Ok(round_numeric_unary(a)),
            TrimWhitespace => Ok(trim_whitespace(a)),
            TrimLeadingWhitespace => Ok(trim_leading_whitespace(a)),
            TrimTrailingWhitespace => Ok(trim_trailing_whitespace(a)),
            RecordGet(i) => Ok(record_get(a, *i)),
            ListLength => Ok(list_length(a)),
            Upper => Ok(upper(a, temp_storage)),
            Lower => Ok(lower(a, temp_storage)),
            Log10Numeric => log_numeric(a, dec::Context::log10, "log10"),
            LnNumeric => log_numeric(a, dec::Context::ln, "ln"),
            ExpNumeric => exp_numeric(a),
            RescaleNumeric(scale) => rescale_numeric(a, *scale),
        }
    }

    fn output_type_manual(&self, input_type: ColumnType) -> ColumnType {
        use UnaryFunc::*;
        let nullable = if self.introduces_nulls() {
            true
        } else if self.propagates_nulls() {
            input_type.nullable
        } else {
            false
        };
        match self {
            Not(_)
            | NegFloat32(_)
            | NegFloat64(_)
            | NegInt16(_)
            | NegInt32(_)
            | NegInt64(_)
            | AbsFloat32(_)
            | AbsFloat64(_)
            | AbsInt16(_)
            | AbsInt32(_)
            | AbsInt64(_)
            | BitNotInt16(_)
            | BitNotInt32(_)
            | BitNotInt64(_)
            | RoundFloat32(_)
            | RoundFloat64(_)
            | CeilFloat32(_)
            | CeilFloat64(_)
            | FloorFloat32(_)
            | FloorFloat64(_)
            | CastFloat32ToInt16(_)
            | CastFloat32ToInt32(_)
            | CastFloat32ToInt64(_)
            | CastFloat64ToInt16(_)
            | CastFloat64ToInt32(_)
            | CastFloat64ToInt64(_)
            | CastFloat64ToFloat32(_)
            | PgColumnSize(_)
            | MzRowSize(_)
            | IsNull(_)
            | IsTrue(_)
            | IsFalse(_)
            | CastFloat32ToString(_)
            | Sleep(_)
            | ToTimestamp(_)
            | CastFloat64ToString(_)
            | Cos(_)
            | Cosh(_)
            | Sin(_)
            | Sinh(_)
            | Tan(_)
            | Tanh(_)
            | Cot(_)
            | Log10(_)
            | Ln(_)
            | Exp(_)
            | SqrtFloat64(_)
            | CbrtFloat64(_)
            | CastFloat32ToFloat64(_) => unreachable!(),

            Ascii | CharLength | BitLengthBytes | BitLengthString | ByteLengthBytes
            | ByteLengthString => ScalarType::Int32.nullable(nullable),

            IsRegexpMatch(_) | CastInt32ToBool | CastInt64ToBool | CastStringToBool => {
                ScalarType::Bool.nullable(nullable)
            }

            CastStringToBytes => ScalarType::Bytes.nullable(nullable),
            CastStringToInterval | CastTimeToInterval => ScalarType::Interval.nullable(nullable),
            CastStringToUuid => ScalarType::Uuid.nullable(nullable),
            CastStringToJsonb => ScalarType::Jsonb.nullable(nullable),

            CastBoolToString
            | CastBoolToStringNonstandard
            | CastCharToString
            | CastVarCharToString
            | CastInt16ToString
            | CastInt32ToString
            | CastInt64ToString
            | CastNumericToString
            | CastDateToString
            | CastTimeToString
            | CastTimestampToString
            | CastTimestampTzToString
            | CastIntervalToString
            | CastBytesToString
            | CastUuidToString
            | CastRecordToString { .. }
            | CastArrayToString { .. }
            | CastListToString { .. }
            | CastMapToString { .. }
            | TrimWhitespace
            | TrimLeadingWhitespace
            | TrimTrailingWhitespace
            | Upper
            | Lower => ScalarType::String.nullable(nullable),

            CastStringToFloat32 | CastInt16ToFloat32 | CastInt32ToFloat32 | CastInt64ToFloat32
            | CastNumericToFloat32 => ScalarType::Float32.nullable(nullable),

            CastStringToFloat64 | CastInt16ToFloat64 | CastInt32ToFloat64 | CastInt64ToFloat64
            | CastNumericToFloat64 => ScalarType::Float64.nullable(nullable),

            CastStringToInt16 | CastInt32ToInt16 | CastInt64ToInt16 | CastNumericToInt16 => {
                ScalarType::Int16.nullable(nullable)
            }

            CastBoolToInt32 | CastStringToInt32 | CastInt16ToInt32 | CastInt64ToInt32
            | CastNumericToInt32 => ScalarType::Int32.nullable(nullable),

            CastStringToInt64 | CastInt16ToInt64 | CastInt32ToInt64 | CastNumericToInt64 => {
                ScalarType::Int64.nullable(nullable)
            }

            CastStringToNumeric(scale)
            | CastInt16ToNumeric(scale)
            | CastInt32ToNumeric(scale)
            | CastInt64ToNumeric(scale)
            | CastFloat32ToNumeric(scale)
            | CastFloat64ToNumeric(scale)
            | CastJsonbToNumeric(scale) => ScalarType::Numeric { scale: *scale }.nullable(nullable),

            CastInt32ToOid => ScalarType::Oid.nullable(nullable),
            CastInt32ToRegProc => ScalarType::RegProc.nullable(nullable),
            CastOidToInt32 => ScalarType::Int32.nullable(nullable),
            CastRegProcToOid => ScalarType::Oid.nullable(nullable),
            CastOidToRegProc => ScalarType::RegProc.nullable(nullable),

            CastStringToDate | CastTimestampToDate | CastTimestampTzToDate => {
                ScalarType::Date.nullable(nullable)
            }

            CastStringToTime | CastIntervalToTime | TimezoneTime { .. } => {
                ScalarType::Time.nullable(nullable)
            }

            CastStringToTimestamp
            | CastDateToTimestamp
            | CastTimestampTzToTimestamp
            | TimezoneTimestampTz(_) => ScalarType::Timestamp.nullable(nullable),

            CastStringToTimestampTz
            | CastDateToTimestampTz
            | CastTimestampToTimestampTz
            | TimezoneTimestamp(_) => ScalarType::TimestampTz.nullable(nullable),

            // converts null to jsonnull
            CastJsonbOrNullToJsonb => ScalarType::Jsonb.nullable(nullable),

            CastJsonbToString => ScalarType::String.nullable(nullable),
            CastJsonbToInt16 => ScalarType::Int16.nullable(nullable),
            CastJsonbToInt32 => ScalarType::Int32.nullable(nullable),
            CastJsonbToInt64 => ScalarType::Int64.nullable(nullable),
            CastJsonbToFloat32 => ScalarType::Float32.nullable(nullable),
            CastJsonbToFloat64 => ScalarType::Float64.nullable(nullable),
            CastJsonbToBool => ScalarType::Bool.nullable(nullable),

            CastStringToArray { return_ty, .. }
            | CastStringToMap { return_ty, .. }
            | CastInPlace { return_ty } => (return_ty.clone()).nullable(nullable),

            CastList1ToList2 { return_ty, .. } | CastStringToList { return_ty, .. } => {
                return_ty.default_embedded_value().nullable(false)
            }

            CastStringToChar { length, .. } | PadChar { length } => {
                ScalarType::Char { length: *length }.nullable(nullable)
            }

            CastStringToVarChar { length, .. } => {
                ScalarType::VarChar { length: *length }.nullable(nullable)
            }

            NegInterval => input_type,

            DatePartInterval(_) | DatePartTimestamp(_) | DatePartTimestampTz(_) => {
                ScalarType::Float64.nullable(nullable)
            }

            DateTruncTimestamp(_) => ScalarType::Timestamp.nullable(nullable),
            DateTruncTimestampTz(_) => ScalarType::TimestampTz.nullable(nullable),

            JsonbArrayLength => ScalarType::Int64.nullable(nullable),
            JsonbTypeof => ScalarType::String.nullable(nullable),
            JsonbStripNulls => ScalarType::Jsonb.nullable(nullable),
            JsonbPretty => ScalarType::String.nullable(nullable),

            RecordGet(i) => match input_type.scalar_type {
                ScalarType::Record { mut fields, .. } => {
                    let (_name, mut ty) = fields.swap_remove(*i);
                    ty.nullable = ty.nullable || input_type.nullable;
                    ty
                }
                _ => unreachable!("RecordGet specified nonexistent field"),
            },

            ListLength => ScalarType::Int64.nullable(nullable),

            RegexpMatch(_) => ScalarType::Array(Box::new(ScalarType::String)).nullable(nullable),

            RescaleNumeric(scale) => (ScalarType::Numeric {
                scale: Some(*scale),
            })
            .nullable(nullable),

            AbsNumeric | CeilNumeric | ExpNumeric | FloorNumeric | LnNumeric | Log10Numeric
            | NegNumeric | RoundNumeric | SqrtNumeric => {
                ScalarType::Numeric { scale: None }.nullable(nullable)
            }
        }
    }

    /// Whether the function output is NULL if any of its inputs are NULL.
    pub fn propagates_nulls_manual(&self) -> bool {
        match self {
            UnaryFunc::Not(_) => unreachable!(),
            // converts null to jsonnull
            UnaryFunc::CastJsonbOrNullToJsonb => false,
            _ => true,
        }
    }

    /// Whether the function might return NULL even if none of its inputs are
    /// NULL.
    pub fn introduces_nulls_manual(&self) -> bool {
        use UnaryFunc::*;
        match self {
            Not(_)
            | NegFloat32(_)
            | NegFloat64(_)
            | NegInt16(_)
            | NegInt32(_)
            | NegInt64(_)
            | AbsFloat32(_)
            | AbsFloat64(_)
            | AbsInt16(_)
            | AbsInt32(_)
            | AbsInt64(_)
            | BitNotInt16(_)
            | BitNotInt32(_)
            | BitNotInt64(_)
            | RoundFloat32(_)
            | RoundFloat64(_)
            | CeilFloat32(_)
            | CeilFloat64(_)
            | FloorFloat32(_)
            | FloorFloat64(_)
            | CastFloat32ToInt16(_)
            | CastFloat32ToInt32(_)
            | CastFloat32ToInt64(_)
            | CastFloat64ToInt16(_)
            | CastFloat64ToInt32(_)
            | CastFloat64ToInt64(_)
            | CastFloat64ToFloat32(_)
            | PgColumnSize(_)
            | MzRowSize(_)
            | IsNull(_)
            | IsTrue(_)
            | IsFalse(_)
            | CastFloat32ToString(_)
            | Sleep(_)
            | ToTimestamp(_)
            | CastFloat64ToString(_)
            | Cos(_)
            | Cosh(_)
            | Sin(_)
            | Sinh(_)
            | Tan(_)
            | Tanh(_)
            | Cot(_)
            | Log10(_)
            | Ln(_)
            | Exp(_)
            | SqrtFloat64(_)
            | CbrtFloat64(_)
            | CastFloat32ToFloat64(_) => unreachable!(),
            // These return null when their input is SQL null.
            CastJsonbToString | CastJsonbToInt16 | CastJsonbToInt32 | CastJsonbToInt64
            | CastJsonbToFloat32 | CastJsonbToFloat64 | CastJsonbToBool => true,
            // Return null if the inner field is null
            RecordGet(_) => true,
            // Always returns null
            // Returns null if the regex did not match
            RegexpMatch(_) => true,
            // Returns null on non-array input
            JsonbArrayLength => true,

            Ascii | CharLength | BitLengthBytes | BitLengthString | ByteLengthBytes
            | ByteLengthString => false,
            IsRegexpMatch(_)
            | CastInt32ToBool
            | CastInt64ToBool
            | CastStringToBool
            | CastJsonbOrNullToJsonb => false,
            CastStringToBytes | CastStringToInterval | CastTimeToInterval | CastStringToJsonb => {
                false
            }
            CastBoolToString
            | CastBoolToStringNonstandard
            | CastCharToString
            | CastVarCharToString
            | CastInt16ToString
            | CastInt32ToString
            | CastInt64ToString
            | CastNumericToString
            | CastDateToString
            | CastTimeToString
            | CastTimestampToString
            | CastTimestampTzToString
            | CastIntervalToString
            | CastBytesToString
            | CastUuidToString
            | CastRecordToString { .. }
            | CastArrayToString { .. }
            | CastListToString { .. }
            | CastMapToString { .. }
            | TrimWhitespace
            | TrimLeadingWhitespace
            | TrimTrailingWhitespace
            | Upper
            | Lower => false,
            CastStringToFloat32 | CastInt32ToFloat32 | CastInt16ToFloat32 | CastInt64ToFloat32
            | CastNumericToFloat32 => false,
            CastStringToFloat64 | CastInt32ToFloat64 | CastInt16ToFloat64 | CastInt64ToFloat64
            | CastNumericToFloat64 => false,
            CastStringToInt16 | CastInt32ToInt16 | CastInt64ToInt16 | CastNumericToInt16 => false,
            CastBoolToInt32 | CastStringToInt32 | CastInt16ToInt32 | CastInt64ToInt32
            | CastNumericToInt32 => false,
            CastStringToInt64 | CastInt16ToInt64 | CastInt32ToInt64 | CastNumericToInt64 => false,
            CastStringToNumeric(_)
            | CastInt16ToNumeric(_)
            | CastInt32ToNumeric(_)
            | CastInt64ToNumeric(_)
            | CastFloat32ToNumeric(_)
            | CastFloat64ToNumeric(_)
            | CastJsonbToNumeric(_) => false,
            CastInt32ToOid | CastOidToInt32 | CastInt32ToRegProc | CastRegProcToOid
            | CastOidToRegProc => false,
            CastStringToDate | CastTimestampToDate | CastTimestampTzToDate => false,
            CastStringToTime | CastIntervalToTime | TimezoneTime { .. } => false,
            CastStringToTimestamp
            | CastDateToTimestamp
            | CastTimestampTzToTimestamp
            | TimezoneTimestampTz(_) => false,
            CastStringToTimestampTz
            | CastDateToTimestampTz
            | CastTimestampToTimestampTz
            | TimezoneTimestamp(_) => false,
            CastStringToUuid => false,
            CastList1ToList2 { .. }
            | CastStringToArray { .. }
            | CastStringToList { .. }
            | CastStringToMap { .. }
            | CastInPlace { .. } => false,
            CastStringToChar { .. } | PadChar { .. } => false,
            CastStringToVarChar { .. } => false,
            JsonbTypeof | JsonbStripNulls | JsonbPretty | ListLength => false,
            DatePartInterval(_) | DatePartTimestamp(_) | DatePartTimestampTz(_) => false,
            DateTruncTimestamp(_) | DateTruncTimestampTz(_) => false,
            NegInterval => false,
            AbsNumeric | CeilNumeric | ExpNumeric | FloorNumeric | LnNumeric | Log10Numeric
            | NegNumeric | RoundNumeric | SqrtNumeric | RescaleNumeric(_) => false,
        }
    }

    /// True iff for x != y, we are assured f(x) != f(y).
    ///
    /// This is most often the case for methods that promote to types that
    /// can contain all the precision of the input type.
    pub fn preserves_uniqueness_manual(&self) -> bool {
        use UnaryFunc::*;
        match self {
            Not(_)
            | NegFloat32(_)
            | NegFloat64(_)
            | NegInt16(_)
            | NegInt32(_)
            | NegInt64(_)
            | AbsFloat32(_)
            | AbsFloat64(_)
            | AbsInt16(_)
            | AbsInt32(_)
            | AbsInt64(_)
            | RoundFloat32(_)
            | RoundFloat64(_)
            | CeilFloat32(_)
            | CeilFloat64(_)
            | FloorFloat32(_)
            | FloorFloat64(_)
            | CastFloat32ToInt16(_)
            | CastFloat32ToInt32(_)
            | CastFloat32ToInt64(_)
            | CastFloat64ToInt16(_)
            | CastFloat64ToInt32(_)
            | CastFloat64ToInt64(_)
            | CastFloat64ToFloat32(_)
            | PgColumnSize(_)
            | MzRowSize(_)
            | IsNull(_)
            | IsTrue(_)
            | IsFalse(_)
            | CastFloat32ToString(_)
            | Sleep(_)
            | ToTimestamp(_)
            | CastFloat64ToString(_)
            | Cos(_)
            | Cosh(_)
            | Sin(_)
            | Sinh(_)
            | Tan(_)
            | Tanh(_)
            | Cot(_)
            | Log10(_)
            | Ln(_)
            | Exp(_)
            | SqrtFloat64(_)
            | CbrtFloat64(_)
            | CastFloat32ToFloat64(_) => unreachable!(),
            NegNumeric
            | CastBoolToString
            | CastBoolToStringNonstandard
            | CastCharToString
            | CastVarCharToString
            | CastInt16ToInt32
            | CastInt16ToInt64
            | CastInt16ToString
            | CastInt32ToInt16
            | CastInt32ToInt64
            | CastInt32ToString
            | CastInt64ToString
            | CastStringToBytes
            | CastDateToTimestamp
            | CastDateToTimestampTz
            | CastDateToString
            | CastTimeToInterval
            | CastTimeToString => true,
            _ => false,
        }
    }

    fn fmt_manual(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use UnaryFunc::*;
        match self {
            Not(_)
            | NegFloat32(_)
            | NegFloat64(_)
            | NegInt16(_)
            | NegInt32(_)
            | NegInt64(_)
            | AbsFloat32(_)
            | AbsFloat64(_)
            | AbsInt16(_)
            | AbsInt32(_)
            | AbsInt64(_)
            | BitNotInt16(_)
            | BitNotInt32(_)
            | BitNotInt64(_)
            | RoundFloat32(_)
            | RoundFloat64(_)
            | CeilFloat32(_)
            | CeilFloat64(_)
            | FloorFloat32(_)
            | FloorFloat64(_)
            | CastFloat32ToInt16(_)
            | CastFloat32ToInt32(_)
            | CastFloat32ToInt64(_)
            | CastFloat64ToInt16(_)
            | CastFloat64ToInt32(_)
            | CastFloat64ToInt64(_)
            | CastFloat64ToFloat32(_)
            | PgColumnSize(_)
            | MzRowSize(_)
            | IsNull(_)
            | IsTrue(_)
            | IsFalse(_)
            | CastFloat32ToString(_)
            | Sleep(_)
            | ToTimestamp(_)
            | CastFloat64ToString(_)
            | Cos(_)
            | Cosh(_)
            | Sin(_)
            | Sinh(_)
            | Tan(_)
            | Tanh(_)
            | Cot(_)
            | Log10(_)
            | Ln(_)
            | Exp(_)
            | SqrtFloat64(_)
            | CbrtFloat64(_)
            | CastFloat32ToFloat64(_) => unreachable!(),
            NegNumeric => f.write_str("-"),
            NegInterval => f.write_str("-"),
            AbsNumeric => f.write_str("abs"),
            CastBoolToString => f.write_str("booltostr"),
            CastBoolToStringNonstandard => f.write_str("booltostrns"),
            CastBoolToInt32 => f.write_str("booltoi32"),
            CastInt16ToFloat32 => f.write_str("i16tof32"),
            CastInt16ToFloat64 => f.write_str("i16tof64"),
            CastInt16ToInt32 => f.write_str("i16toi32"),
            CastInt16ToInt64 => f.write_str("i16toi64"),
            CastInt16ToString => f.write_str("i16tostr"),
            CastInt16ToNumeric(..) => f.write_str("i16tonumeric"),
            CastInt32ToBool => f.write_str("i32tobool"),
            CastInt32ToFloat32 => f.write_str("i32tof32"),
            CastInt32ToFloat64 => f.write_str("i32tof64"),
            CastInt32ToInt16 => f.write_str("i32toi16"),
            CastInt32ToInt64 => f.write_str("i32toi64"),
            CastInt32ToOid => f.write_str("i32tooid"),
            CastInt32ToRegProc => f.write_str("i32toregproc"),
            CastInt32ToString => f.write_str("i32tostr"),
            CastInt32ToNumeric(..) => f.write_str("i32tonumeric"),
            CastOidToInt32 => f.write_str("oidtoi32"),
            CastRegProcToOid => f.write_str("regproctooid"),
            CastOidToRegProc => f.write_str("oidtoregproc"),
            CastInt64ToInt16 => f.write_str("i64toi16"),
            CastInt64ToInt32 => f.write_str("i64toi32"),
            CastInt64ToBool => f.write_str("i64tobool"),
            CastInt64ToNumeric(..) => f.write_str("i64tonumeric"),
            CastInt64ToFloat32 => f.write_str("i64tof32"),
            CastInt64ToFloat64 => f.write_str("i64tof64"),
            CastInt64ToString => f.write_str("i64tostr"),
            CastFloat32ToNumeric(_) => f.write_str("f32tonumeric"),
            CastFloat64ToNumeric(_) => f.write_str("f32tonumeric"),
            CastNumericToInt16 => f.write_str("numerictoi16"),
            CastNumericToInt32 => f.write_str("numerictoi32"),
            CastNumericToInt64 => f.write_str("numerictoi64"),
            CastNumericToString => f.write_str("numerictostr"),
            CastNumericToFloat32 => f.write_str("numerictof32"),
            CastNumericToFloat64 => f.write_str("numerictof64"),
            CastStringToBool => f.write_str("strtobool"),
            CastStringToBytes => f.write_str("strtobytes"),
            CastStringToInt16 => f.write_str("strtoi16"),
            CastStringToInt32 => f.write_str("strtoi32"),
            CastStringToInt64 => f.write_str("strtoi64"),
            CastStringToFloat32 => f.write_str("strtof32"),
            CastStringToFloat64 => f.write_str("strtof64"),
            CastStringToNumeric(_) => f.write_str("strtonumeric"),
            CastStringToDate => f.write_str("strtodate"),
            CastStringToArray { .. } => f.write_str("strtoarray"),
            CastStringToList { .. } => f.write_str("strtolist"),
            CastStringToMap { .. } => f.write_str("strtomap"),
            CastStringToTime => f.write_str("strtotime"),
            CastStringToTimestamp => f.write_str("strtots"),
            CastStringToTimestampTz => f.write_str("strtotstz"),
            CastStringToInterval => f.write_str("strtoiv"),
            CastStringToUuid => f.write_str("strtouuid"),
            CastStringToChar { .. } => f.write_str("strtochar"),
            PadChar { .. } => f.write_str("padchar"),
            CastCharToString => f.write_str("chartostr"),
            CastVarCharToString => f.write_str("varchartostr"),
            CastStringToVarChar { .. } => f.write_str("strtovarchar"),
            CastDateToTimestamp => f.write_str("datetots"),
            CastDateToTimestampTz => f.write_str("datetotstz"),
            CastDateToString => f.write_str("datetostr"),
            CastTimeToInterval => f.write_str("timetoiv"),
            CastTimeToString => f.write_str("timetostr"),
            CastTimestampToDate => f.write_str("tstodate"),
            CastTimestampToTimestampTz => f.write_str("tstotstz"),
            CastTimestampToString => f.write_str("tstostr"),
            CastTimestampTzToDate => f.write_str("tstodate"),
            CastTimestampTzToTimestamp => f.write_str("tstztots"),
            CastTimestampTzToString => f.write_str("tstztostr"),
            CastIntervalToString => f.write_str("ivtostr"),
            CastIntervalToTime => f.write_str("ivtotime"),
            CastBytesToString => f.write_str("bytestostr"),
            CastStringToJsonb => f.write_str("strtojsonb"),
            CastJsonbOrNullToJsonb => f.write_str("jsonb?tojsonb"),
            CastJsonbToString => f.write_str("jsonbtostr"),
            CastJsonbToInt16 => f.write_str("jsonbtoi16"),
            CastJsonbToInt32 => f.write_str("jsonbtoi32"),
            CastJsonbToInt64 => f.write_str("jsonbtoi64"),
            CastJsonbToFloat32 => f.write_str("jsonbtof32"),
            CastJsonbToFloat64 => f.write_str("jsonbtof64"),
            CastJsonbToBool => f.write_str("jsonbtobool"),
            CastJsonbToNumeric(_) => f.write_str("jsonbtonumeric"),
            CastUuidToString => f.write_str("uuidtostr"),
            CastRecordToString { .. } => f.write_str("recordtostr"),
            CastArrayToString { .. } => f.write_str("arraytostr"),
            CastListToString { .. } => f.write_str("listtostr"),
            CastList1ToList2 { .. } => f.write_str("list1tolist2"),
            CastMapToString { .. } => f.write_str("maptostr"),
            CastInPlace { .. } => f.write_str("castinplace"),
            CeilNumeric => f.write_str("ceilnumeric"),
            FloorNumeric => f.write_str("floornumeric"),
            SqrtNumeric => f.write_str("sqrtnumeric"),
            Ascii => f.write_str("ascii"),
            CharLength => f.write_str("char_length"),
            BitLengthBytes => f.write_str("bit_length"),
            BitLengthString => f.write_str("bit_length"),
            ByteLengthBytes => f.write_str("octet_length"),
            ByteLengthString => f.write_str("octet_length"),
            IsRegexpMatch(regex) => write!(f, "{} ~", regex.as_str().quoted()),
            RegexpMatch(regex) => write!(f, "regexp_match[{}]", regex.as_str()),
            DatePartInterval(units) => write!(f, "date_part_{}_iv", units),
            DatePartTimestamp(units) => write!(f, "date_part_{}_ts", units),
            DatePartTimestampTz(units) => write!(f, "date_part_{}_tstz", units),
            DateTruncTimestamp(units) => write!(f, "date_trunc_{}_ts", units),
            DateTruncTimestampTz(units) => write!(f, "date_trunc_{}_tstz", units),
            TimezoneTimestamp(tz) => write!(f, "timezone_{}_ts", tz),
            TimezoneTimestampTz(tz) => write!(f, "timezone_{}_tstz", tz),
            TimezoneTime { tz, .. } => write!(f, "timezone_{}_t", tz),
            JsonbArrayLength => f.write_str("jsonb_array_length"),
            JsonbTypeof => f.write_str("jsonb_typeof"),
            JsonbStripNulls => f.write_str("jsonb_strip_nulls"),
            JsonbPretty => f.write_str("jsonb_pretty"),
            RoundNumeric => f.write_str("roundnumeric"),
            TrimWhitespace => f.write_str("btrim"),
            TrimLeadingWhitespace => f.write_str("ltrim"),
            TrimTrailingWhitespace => f.write_str("rtrim"),
            RecordGet(i) => write!(f, "record_get[{}]", i),
            ListLength => f.write_str("list_length"),
            Upper => f.write_str("upper"),
            Lower => f.write_str("lower"),
            Log10Numeric => f.write_str("log10numeric"),
            LnNumeric => f.write_str("lnnumeric"),
            ExpNumeric => f.write_str("expnumeric"),
            RescaleNumeric(..) => f.write_str("rescale_numeric"),
        }
    }
}

fn coalesce<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
    exprs: &'a [MirScalarExpr],
) -> Result<Datum<'a>, EvalError> {
    for e in exprs {
        let d = e.eval(datums, temp_storage)?;
        if !d.is_null() {
            return Ok(d);
        }
    }
    Ok(Datum::Null)
}

fn error_if_null<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
    exprs: &'a [MirScalarExpr],
) -> Result<Datum<'a>, EvalError> {
    let datums = exprs
        .iter()
        .map(|e| e.eval(datums, temp_storage))
        .collect::<Result<Vec<_>, _>>()?;
    match datums[0] {
        Datum::Null => Err(EvalError::Internal(datums[1].unwrap_str().to_string())),
        _ => Ok(datums[0]),
    }
}

fn text_concat_binary<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    buf.push_str(a.unwrap_str());
    buf.push_str(b.unwrap_str());
    Datum::String(temp_storage.push_string(buf))
}

fn text_concat_variadic<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    for d in datums {
        if !d.is_null() {
            buf.push_str(d.unwrap_str());
        }
    }
    Datum::String(temp_storage.push_string(buf))
}

fn pad_leading<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let string = datums[0].unwrap_str();

    let len = match usize::try_from(datums[1].unwrap_int64()) {
        Ok(len) => len,
        Err(_) => {
            return Err(EvalError::InvalidParameterValue(
                "length must be nonnegative".to_owned(),
            ))
        }
    };

    let pad_string = if datums.len() == 3 {
        datums[2].unwrap_str()
    } else {
        " "
    };

    let (end_char, end_char_byte_offset) = string
        .chars()
        .take(len)
        .fold((0, 0), |acc, char| (acc.0 + 1, acc.1 + char.len_utf8()));

    let mut buf = String::with_capacity(len);
    if len == end_char {
        buf.push_str(&string[0..end_char_byte_offset]);
    } else {
        buf.extend(pad_string.chars().cycle().take(len - end_char));
        buf.push_str(string);
    }

    Ok(Datum::String(temp_storage.push_string(buf)))
}

fn substr<'a>(datums: &[Datum<'a>]) -> Result<Datum<'a>, EvalError> {
    let s: &'a str = datums[0].unwrap_str();

    let raw_start_idx = datums[1].unwrap_int64() - 1;
    let start_idx = match usize::try_from(cmp::max(raw_start_idx, 0)) {
        Ok(i) => i,
        Err(_) => {
            return Err(EvalError::InvalidParameterValue(format!(
                "substring starting index ({}) exceeds min/max position",
                raw_start_idx
            )))
        }
    } as usize;

    let mut char_indices = s.char_indices();
    let get_str_index = |(index, _char)| index;

    let str_len = s.len();
    let start_char_idx = char_indices
        .nth(start_idx as usize)
        .map_or(str_len, &get_str_index);

    if datums.len() == 3 {
        let end_idx = match datums[2].unwrap_int64() {
            e if e < 0 => {
                return Err(EvalError::InvalidParameterValue(
                    "negative substring length not allowed".to_owned(),
                ))
            }
            e if e == 0 || e + raw_start_idx < 1 => return Ok(Datum::String("")),
            e => {
                let e = cmp::min(raw_start_idx + e - 1, e - 1);
                match usize::try_from(e) {
                    Ok(i) => i,
                    Err(_) => {
                        return Err(EvalError::InvalidParameterValue(format!(
                            "substring length ({}) exceeds max position",
                            e
                        )))
                    }
                }
            }
        };

        let end_char_idx = char_indices.nth(end_idx).map_or(str_len, &get_str_index);

        Ok(Datum::String(&s[start_char_idx..end_char_idx]))
    } else {
        Ok(Datum::String(&s[start_char_idx..]))
    }
}

fn split_part<'a>(datums: &[Datum<'a>]) -> Result<Datum<'a>, EvalError> {
    let string = datums[0].unwrap_str();
    let delimiter = datums[1].unwrap_str();

    // Provided index value begins at 1, not 0.
    let index = match usize::try_from(datums[2].unwrap_int64() - 1) {
        Ok(index) => index,
        Err(_) => {
            return Err(EvalError::InvalidParameterValue(
                "field position must be greater than zero".to_owned(),
            ))
        }
    };

    // If the provided delimiter is the empty string,
    // PostgreSQL does not break the string into individual
    // characters. Instead, it generates the following parts: [string].
    if delimiter.is_empty() {
        if index == 0 {
            return Ok(datums[0]);
        } else {
            return Ok(Datum::String(""));
        }
    }

    // If provided index is greater than the number of split parts,
    // return an empty string.
    Ok(Datum::String(
        string.split(delimiter).nth(index).unwrap_or(""),
    ))
}

fn is_like_pattern_match_dynamic<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    case_insensitive: bool,
) -> Result<Datum<'a>, EvalError> {
    let haystack = a.unwrap_str();
    let flags = if case_insensitive { "i" } else { "" };
    let needle = like_pattern::build_regex(b.unwrap_str(), flags)?;
    Ok(Datum::from(needle.is_match(haystack.as_ref())))
}

fn is_regexp_match_static<'a>(a: Datum<'a>, needle: &regex::Regex) -> Datum<'a> {
    let haystack = a.unwrap_str();
    Datum::from(needle.is_match(haystack))
}

fn is_regexp_match_dynamic<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    case_insensitive: bool,
) -> Result<Datum<'a>, EvalError> {
    let haystack = a.unwrap_str();
    let needle = build_regex(b.unwrap_str(), if case_insensitive { "i" } else { "" })?;
    Ok(Datum::from(needle.is_match(haystack)))
}

fn regexp_match_dynamic<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let haystack = datums[0];
    let needle = datums[1].unwrap_str();
    let flags = match datums.get(2) {
        Some(d) => d.unwrap_str(),
        None => "",
    };
    let needle = build_regex(needle, flags)?;
    regexp_match_static(haystack, temp_storage, &needle)
}

fn regexp_match_static<'a>(
    haystack: Datum<'a>,
    temp_storage: &'a RowArena,
    needle: &regex::Regex,
) -> Result<Datum<'a>, EvalError> {
    let mut row = Row::default();
    if needle.captures_len() > 1 {
        // The regex contains capture groups, so return an array containing the
        // matched text in each capture group, unless the entire match fails.
        // Individual capture groups may also be null if that group did not
        // participate in the match.
        match needle.captures(haystack.unwrap_str()) {
            None => row.push(Datum::Null),
            Some(captures) => row.push_array(
                &[ArrayDimension {
                    lower_bound: 1,
                    length: captures.len() - 1,
                }],
                // Skip the 0th capture group, which is the whole match.
                captures.iter().skip(1).map(|mtch| match mtch {
                    None => Datum::Null,
                    Some(mtch) => Datum::String(mtch.as_str()),
                }),
            )?,
        }
    } else {
        // The regex contains no capture groups, so return a one-element array
        // containing the match, or null if there is no match.
        match needle.find(haystack.unwrap_str()) {
            None => row.push(Datum::Null),
            Some(mtch) => row.push_array(
                &[ArrayDimension {
                    lower_bound: 1,
                    length: 1,
                }],
                iter::once(Datum::String(mtch.as_str())),
            )?,
        };
    };
    Ok(temp_storage.push_unary_row(row))
}

pub fn build_regex(needle: &str, flags: &str) -> Result<regex::Regex, EvalError> {
    let mut regex = RegexBuilder::new(needle);
    for f in flags.chars() {
        match f {
            'i' => {
                regex.case_insensitive(true);
            }
            'c' => {
                regex.case_insensitive(false);
            }
            _ => return Err(EvalError::InvalidRegexFlag(f)),
        }
    }
    Ok(regex.build()?)
}

pub fn hmac_string<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let to_digest = datums[0].unwrap_str().as_bytes();
    let key = datums[1].unwrap_str().as_bytes();
    let typ = datums[2].unwrap_str();
    hmac_inner(to_digest, key, typ, temp_storage)
}

pub fn hmac_bytes<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let to_digest = datums[0].unwrap_bytes();
    let key = datums[1].unwrap_bytes();
    let typ = datums[2].unwrap_str();
    hmac_inner(to_digest, key, typ, temp_storage)
}

pub fn hmac_inner<'a>(
    to_digest: &[u8],
    key: &[u8],
    typ: &str,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let bytes = match typ {
        "md5" => {
            let mut mac = Hmac::<Md5>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        "sha1" => {
            let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        "sha224" => {
            let mut mac = Hmac::<Sha224>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        "sha256" => {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        "sha384" => {
            let mut mac = Hmac::<Sha384>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        "sha512" => {
            let mut mac = Hmac::<Sha512>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        other => return Err(EvalError::InvalidHashAlgorithm(other.to_owned())),
    };
    Ok(Datum::Bytes(temp_storage.push_bytes(bytes)))
}

fn repeat_string<'a>(
    string: Datum<'a>,
    count: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    Ok(Datum::String(
        temp_storage.push_string(
            string
                .unwrap_str()
                .repeat(usize::try_from(count.unwrap_int32()).unwrap_or(0)),
        ),
    ))
}

fn replace<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    Datum::String(
        temp_storage.push_string(
            datums[0]
                .unwrap_str()
                .replace(datums[1].unwrap_str(), datums[2].unwrap_str()),
        ),
    )
}

fn jsonb_build_array<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    if datums.iter().any(|datum| datum.is_null()) {
        // the inputs should all be valid jsonb types, but a casting error might produce a Datum::Null that needs to be propagated
        Datum::Null
    } else {
        temp_storage.make_datum(|packer| packer.push_list(datums))
    }
}

fn jsonb_build_object<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    if datums.iter().any(|datum| datum.is_null()) {
        // the inputs should all be valid jsonb types, but a casting error might produce a Datum::Null that needs to be propagated
        Datum::Null
    } else {
        let mut kvs = datums.chunks(2).collect::<Vec<_>>();
        kvs.sort_by(|kv1, kv2| kv1[0].cmp(&kv2[0]));
        kvs.dedup_by(|kv1, kv2| kv1[0] == kv2[0]);
        temp_storage.make_datum(|packer| {
            packer.push_dict(kvs.into_iter().map(|kv| (kv[0].unwrap_str(), kv[1])))
        })
    }
}

/// Constructs a new multidimensional array out of an arbitrary number of
/// lower-dimensional arrays.
///
/// For example, if given three 1D arrays of length 2, this function will
/// construct a 2D array with dimensions 3x2.
///
/// The input datums in `datums` must all be arrays of the same dimensions.
/// (The arrays must also be of the same element type, but that is checked by
/// the SQL type system, rather than checked here at runtime.)
///
/// If all input arrays are zero-dimensional arrays, then the output is a zero-
/// dimensional array. Otherwise the lower bound of the additional dimension is
/// one and the length of the new dimension is equal to `datums.len()`.
fn array_create_multidim<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    // Per PostgreSQL, if all input arrays are zero dimensional, so is the
    // output.
    if datums.iter().all(|d| d.unwrap_array().dims().is_empty()) {
        let dims = &[];
        let datums = &[];
        let datum = temp_storage.try_make_datum(|packer| packer.push_array(dims, datums))?;
        return Ok(datum);
    }

    let mut dims = vec![ArrayDimension {
        lower_bound: 1,
        length: datums.len(),
    }];
    if let Some(d) = datums.first() {
        dims.extend(d.unwrap_array().dims());
    };
    let elements = datums
        .iter()
        .flat_map(|d| d.unwrap_array().elements().iter());
    let datum = temp_storage.try_make_datum(move |packer| packer.push_array(&dims, elements))?;
    Ok(datum)
}

/// Constructs a new zero or one dimensional array out of an arbitrary number of
/// scalars.
///
/// If `datums` is empty, constructs a zero-dimensional array. Otherwise,
/// constructs a one dimensional array whose lower bound is one and whose length
/// is equal to `datums.len()`.
fn array_create_scalar<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let mut dims = &[ArrayDimension {
        lower_bound: 1,
        length: datums.len(),
    }][..];
    if datums.is_empty() {
        // Per PostgreSQL, empty arrays are represented with zero dimensions,
        // not one dimension of zero length. We write this condition a little
        // strangely to satisfy the borrow checker while avoiding an allocation.
        dims = &[];
    }
    let datum = temp_storage.try_make_datum(|packer| packer.push_array(dims, datums))?;
    Ok(datum)
}

fn array_to_string<'a>(
    datums: &[Datum<'a>],
    elem_type: &ScalarType,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    if datums[0].is_null() || datums[1].is_null() {
        return Ok(Datum::Null);
    }
    let array = datums[0].unwrap_array();
    let delimiter = datums[1].unwrap_str();
    let null_str = match datums.get(2) {
        None | Some(Datum::Null) => None,
        Some(d) => Some(d.unwrap_str()),
    };

    let mut out = String::new();
    for elem in array.elements().iter() {
        if elem.is_null() {
            if let Some(null_str) = null_str {
                out.push_str(null_str);
                out.push_str(delimiter);
            }
        } else {
            stringify_datum(&mut out, elem, elem_type);
            out.push_str(delimiter);
        }
    }
    out.truncate(out.len() - delimiter.len()); // lop off last delimiter
    Ok(Datum::String(temp_storage.push_string(out)))
}

fn list_create<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    temp_storage.make_datum(|packer| packer.push_list(datums))
}

fn cast_collection_to_string<'a>(
    a: Datum,
    ty: &ScalarType,
    temp_storage: &'a RowArena,
) -> Datum<'a> {
    let mut buf = String::new();
    stringify_datum(&mut buf, a, ty);
    Datum::String(temp_storage.push_string(buf))
}

fn stringify_datum<'a, B>(buf: &mut B, d: Datum<'a>, ty: &ScalarType) -> strconv::Nestable
where
    B: FormatBuffer,
{
    use ScalarType::*;
    match &ty {
        Bool => strconv::format_bool(buf, d.unwrap_bool()),
        Int16 => strconv::format_int16(buf, d.unwrap_int16()),
        Int32 | Oid | RegProc => strconv::format_int32(buf, d.unwrap_int32()),
        Int64 => strconv::format_int64(buf, d.unwrap_int64()),
        Float32 => strconv::format_float32(buf, d.unwrap_float32()),
        Float64 => strconv::format_float64(buf, d.unwrap_float64()),
        Numeric { scale } => {
            let mut d = d.unwrap_numeric();
            if let Some(scale) = scale {
                numeric::rescale(&mut d.0, *scale).unwrap();
            }

            strconv::format_numeric(buf, &d)
        }
        Date => strconv::format_date(buf, d.unwrap_date()),
        Time => strconv::format_time(buf, d.unwrap_time()),
        Timestamp => strconv::format_timestamp(buf, d.unwrap_timestamp()),
        TimestampTz => strconv::format_timestamptz(buf, d.unwrap_timestamptz()),
        Interval => strconv::format_interval(buf, d.unwrap_interval()),
        Bytes => strconv::format_bytes(buf, d.unwrap_bytes()),
        String | VarChar { .. } => strconv::format_string(buf, d.unwrap_str()),
        Char { length } => strconv::format_string(
            buf,
            &repr::adt::char::format_str_pad(d.unwrap_str(), *length),
        ),
        Jsonb => strconv::format_jsonb(buf, JsonbRef::from_datum(d)),
        Uuid => strconv::format_uuid(buf, d.unwrap_uuid()),
        Record { fields, .. } => {
            let mut fields = fields.iter();
            strconv::format_record(buf, &d.unwrap_list(), |buf, d| {
                let (_name, ty) = fields.next().unwrap();
                if d.is_null() {
                    buf.write_null()
                } else {
                    stringify_datum(buf.nonnull_buffer(), d, &ty.scalar_type)
                }
            })
        }
        Array(elem_type) => strconv::format_array(
            buf,
            &d.unwrap_array().dims().into_iter().collect::<Vec<_>>(),
            &d.unwrap_array().elements(),
            |buf, d| {
                if d.is_null() {
                    buf.write_null()
                } else {
                    stringify_datum(buf.nonnull_buffer(), d, elem_type)
                }
            },
        ),
        List { element_type, .. } => strconv::format_list(buf, &d.unwrap_list(), |buf, d| {
            if d.is_null() {
                buf.write_null()
            } else {
                stringify_datum(buf.nonnull_buffer(), d, element_type)
            }
        }),
        Map { value_type, .. } => strconv::format_map(buf, &d.unwrap_map(), |buf, d| {
            if d.is_null() {
                buf.write_null()
            } else {
                stringify_datum(buf.nonnull_buffer(), d, value_type)
            }
        }),
    }
}

fn list_slice<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    // Return value indicates whether this level's slices are empty results.
    fn slice_and_descend(d: Datum, ranges: &[(usize, usize)], row: &mut Row) -> bool {
        match ranges {
            [(start, n), ranges @ ..] if !d.is_null() => {
                let mut iter = d.unwrap_list().iter().skip(*start).take(*n).peekable();
                if iter.peek().is_none() {
                    row.push(Datum::Null);
                    true
                } else {
                    let mut empty_results = true;
                    let start = row.data().len();
                    row.push_list_with(|row| {
                        for d in iter {
                            // Determine if all higher-dimension slices produced empty results.
                            empty_results = slice_and_descend(d, ranges, row) && empty_results;
                        }
                    });

                    if empty_results {
                        // If all results were empty, delete the list, insert a
                        // NULL, and notify lower-order slices that your results
                        // were empty.

                        // SAFETY: `start` points to a datum boundary because a)
                        // it comes from a call to `row.data().len()` above,
                        // and b) recursive calls to `slice_and_descend` will
                        // not shrink the row. (The recursive calls may write
                        // data and then erase that data, but a recursive call
                        // will never erase data that it did not write itself.)
                        unsafe { row.truncate(start) }
                        row.push(Datum::Null);
                    }
                    empty_results
                }
            }
            _ => {
                row.push(d);
                // Slicing a NULL produces an empty result.
                d.is_null() && ranges.len() > 0
            }
        }
    }

    assert_eq!(
        datums.len() % 2,
        1,
        "expr::scalar::func::list_slice expects an odd number of arguments; 1 for list + 2 \
        for each start-end pair"
    );
    assert!(
        datums.len() > 2,
        "expr::scalar::func::list_slice expects at least 3 arguments; 1 for list + at least \
        one start-end pair"
    );

    let mut ranges = Vec::new();
    for (start, end) in datums[1..].iter().tuples::<(_, _)>() {
        let start = std::cmp::max(start.unwrap_int64(), 1);
        let end = end.unwrap_int64();

        if start > end {
            return Datum::Null;
        }

        ranges.push((start as usize - 1, (end - start) as usize + 1));
    }

    temp_storage.make_datum(|row| {
        slice_and_descend(datums[0], &ranges, row);
    })
}

fn record_get(a: Datum, i: usize) -> Datum {
    a.unwrap_list().iter().nth(i).unwrap()
}

fn list_length(a: Datum) -> Datum {
    Datum::Int64(a.unwrap_list().iter().count() as i64)
}

fn upper<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    Datum::String(temp_storage.push_string(a.unwrap_str().to_owned().to_uppercase()))
}

fn lower<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    Datum::String(temp_storage.push_string(a.unwrap_str().to_owned().to_lowercase()))
}

fn make_timestamp<'a>(datums: &[Datum<'a>]) -> Datum<'a> {
    let year: i32 = match datums[0].unwrap_int64().try_into() {
        Ok(year) => year,
        Err(_) => return Datum::Null,
    };
    let month: u32 = match datums[1].unwrap_int64().try_into() {
        Ok(month) => month,
        Err(_) => return Datum::Null,
    };
    let day: u32 = match datums[2].unwrap_int64().try_into() {
        Ok(day) => day,
        Err(_) => return Datum::Null,
    };
    let hour: u32 = match datums[3].unwrap_int64().try_into() {
        Ok(day) => day,
        Err(_) => return Datum::Null,
    };
    let minute: u32 = match datums[4].unwrap_int64().try_into() {
        Ok(day) => day,
        Err(_) => return Datum::Null,
    };
    let second_float = datums[5].unwrap_float64();
    let second = second_float as u32;
    let micros = ((second_float - second as f64) * 1_000_000.0) as u32;
    let date = match NaiveDate::from_ymd_opt(year, month, day) {
        Some(date) => date,
        None => return Datum::Null,
    };
    let timestamp = match date.and_hms_micro_opt(hour, minute, second, micros) {
        Some(timestamp) => timestamp,
        None => return Datum::Null,
    };
    Datum::Timestamp(timestamp)
}

fn trim_whitespace<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_str().trim_matches(' '))
}

fn position<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let substring: &'a str = a.unwrap_str();
    let string = b.unwrap_str();
    let char_index = string.find(substring);

    if let Some(char_index) = char_index {
        // find the index in char space
        let string_prefix = &string[0..char_index];

        let num_prefix_chars = string_prefix.chars().count();
        let num_prefix_chars =
            i32::try_from(num_prefix_chars).map_err(|_| EvalError::Int32OutOfRange)?;

        Ok(Datum::Int32(num_prefix_chars + 1))
    } else {
        Ok(Datum::Int32(0))
    }
}

fn left<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let string: &'a str = a.unwrap_str();
    let n = i64::from(b.unwrap_int32());

    let mut byte_indices = string.char_indices().map(|(i, _)| i);

    let end_in_bytes = match n.cmp(&0) {
        Ordering::Equal => 0,
        Ordering::Greater => {
            let n = usize::try_from(n).map_err(|_| {
                EvalError::InvalidParameterValue(format!("invalid parameter n: {:?}", n))
            })?;
            // nth from the back
            byte_indices.nth(n).unwrap_or_else(|| string.len())
        }
        Ordering::Less => {
            let n = usize::try_from(n.abs() - 1).map_err(|_| {
                EvalError::InvalidParameterValue(format!("invalid parameter n: {:?}", n))
            })?;
            byte_indices.rev().nth(n).unwrap_or(0)
        }
    };

    Ok(Datum::String(&string[..end_in_bytes]))
}

fn right<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let string: &'a str = a.unwrap_str();
    let n = b.unwrap_int32();

    let mut byte_indices = string.char_indices().map(|(i, _)| i);

    let start_in_bytes = if n == 0 {
        string.len()
    } else if n > 0 {
        let n = usize::try_from(n - 1).map_err(|_| {
            EvalError::InvalidParameterValue(format!("invalid parameter n: {:?}", n))
        })?;
        // nth from the back
        byte_indices.rev().nth(n).unwrap_or(0)
    } else if n == i32::MIN {
        // this seems strange but Postgres behaves like this
        0
    } else {
        let n = n.abs();
        let n = usize::try_from(n).map_err(|_| {
            EvalError::InvalidParameterValue(format!("invalid parameter n: {:?}", n))
        })?;
        byte_indices.nth(n).unwrap_or_else(|| string.len())
    };

    Ok(Datum::String(&string[start_in_bytes..]))
}

fn trim<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let trim_chars = b.unwrap_str();

    Datum::from(a.unwrap_str().trim_matches(|c| trim_chars.contains(c)))
}

fn trim_leading_whitespace<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_str().trim_start_matches(' '))
}

fn trim_leading<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let trim_chars = b.unwrap_str();

    Datum::from(
        a.unwrap_str()
            .trim_start_matches(|c| trim_chars.contains(c)),
    )
}

fn trim_trailing_whitespace<'a>(a: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_str().trim_end_matches(' '))
}

fn trim_trailing<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let trim_chars = b.unwrap_str();

    Datum::from(a.unwrap_str().trim_end_matches(|c| trim_chars.contains(c)))
}

fn list_index<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let i = b.unwrap_int64();
    if i < 1 {
        return Datum::Null;
    }
    a.unwrap_list()
        .iter()
        .nth(i as usize - 1)
        .unwrap_or(Datum::Null)
}

fn array_length<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let i = match usize::try_from(b.unwrap_int64()) {
        Ok(0) | Err(_) => return Datum::Null,
        Ok(n) => n - 1,
    };
    match a.unwrap_array().dims().into_iter().nth(i) {
        None => Datum::Null,
        Some(dim) => Datum::Int64(dim.length as i64),
    }
}

fn array_index<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let i = b.unwrap_int64();
    if i < 1 {
        return Datum::Null;
    }
    a.unwrap_array()
        .elements()
        .iter()
        .nth(i as usize - 1)
        .unwrap_or(Datum::Null)
}

fn array_lower<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let i = b.unwrap_int64();
    if i < 1 {
        return Datum::Null;
    }
    match a.unwrap_array().dims().into_iter().nth(i as usize - 1) {
        Some(_) => Datum::Int64(1),
        None => Datum::Null,
    }
}

fn array_upper<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let i = b.unwrap_int64();
    if i < 1 {
        return Datum::Null;
    }
    match a.unwrap_array().dims().into_iter().nth(i as usize - 1) {
        Some(dim) => Datum::Int64(dim.length as i64),
        None => Datum::Null,
    }
}

fn list_length_max<'a>(a: Datum<'a>, b: Datum<'a>, max_dim: usize) -> Result<Datum<'a>, EvalError> {
    fn max_len_on_dim<'a>(d: Datum<'a>, on_dim: i64) -> Option<i64> {
        match d {
            Datum::List(i) => {
                let mut i = i.iter();
                if on_dim > 1 {
                    let mut max_len = None;
                    while let Some(Datum::List(i)) = i.next() {
                        max_len =
                            std::cmp::max(max_len_on_dim(Datum::List(i), on_dim - 1), max_len);
                    }
                    max_len
                } else {
                    Some(i.count() as i64)
                }
            }
            Datum::Null => None,
            _ => unreachable!(),
        }
    }

    let b = b.unwrap_int64();

    if b as usize > max_dim || b < 1 {
        Err(EvalError::InvalidDimension { max_dim, val: b })
    } else {
        Ok(match max_len_on_dim(a, b) {
            Some(l) => Datum::from(l),
            None => Datum::Null,
        })
    }
}

fn array_contains<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let array = Datum::unwrap_array(&b);
    Datum::from(array.elements().iter().any(|e| e == a))
}

fn list_list_concat<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    if a.is_null() {
        return b;
    } else if b.is_null() {
        return a;
    }

    let a = a.unwrap_list().iter();
    let b = b.unwrap_list().iter();

    temp_storage.make_datum(|packer| packer.push_list(a.chain(b)))
}

fn list_element_concat<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    temp_storage.make_datum(|packer| {
        packer.push_list_with(|packer| {
            if !a.is_null() {
                for elem in a.unwrap_list().iter() {
                    packer.push(elem);
                }
            }
            packer.push(b);
        })
    })
}

fn element_list_concat<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    temp_storage.make_datum(|packer| {
        packer.push_list_with(|packer| {
            packer.push(a);
            if !b.is_null() {
                for elem in b.unwrap_list().iter() {
                    packer.push(elem);
                }
            }
        })
    })
}

fn digest_string<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let to_digest = a.unwrap_str().as_bytes();
    digest_inner(to_digest, b, temp_storage)
}

fn digest_bytes<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let to_digest = a.unwrap_bytes();
    digest_inner(to_digest, b, temp_storage)
}

fn digest_inner<'a>(
    bytes: &[u8],
    digest_fn: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let bytes = match digest_fn.unwrap_str() {
        "md5" => Md5::digest(bytes).to_vec(),
        "sha1" => Sha1::digest(bytes).to_vec(),
        "sha224" => Sha224::digest(bytes).to_vec(),
        "sha256" => Sha256::digest(bytes).to_vec(),
        "sha384" => Sha384::digest(bytes).to_vec(),
        "sha512" => Sha512::digest(bytes).to_vec(),
        other => return Err(EvalError::InvalidHashAlgorithm(other.to_owned())),
    };
    Ok(Datum::Bytes(temp_storage.push_bytes(bytes)))
}

fn mz_render_typemod<'a>(
    oid: Datum<'a>,
    typmod: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Datum<'a> {
    let oid = oid.unwrap_int32();
    let mut typmod = typmod.unwrap_int32();
    let typmod_base = 65_536;

    let inner = if matches!(Type::from_oid(oid as u32), Some(Type::Numeric)) && typmod >= 0 {
        typmod -= 4;
        if typmod < 0 {
            temp_storage.push_string(format!("({},{})", 65_535, typmod_base + typmod))
        } else {
            temp_storage.push_string(format!(
                "({},{})",
                typmod / typmod_base,
                typmod % typmod_base
            ))
        }
    } else {
        ""
    };

    Datum::String(inner)
}

#[derive(
    Ord, PartialOrd, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash, MzEnumReflect,
)]
pub enum VariadicFunc {
    Coalesce,
    Concat,
    MakeTimestamp,
    PadLeading,
    Substr,
    Replace,
    JsonbBuildArray,
    JsonbBuildObject,
    ArrayCreate {
        // We need to know the element type to type empty arrays.
        elem_type: ScalarType,
    },
    ArrayToString {
        elem_type: ScalarType,
    },
    ListCreate {
        // We need to know the element type to type empty lists.
        elem_type: ScalarType,
    },
    RecordCreate {
        field_names: Vec<ColumnName>,
    },
    ListSlice,
    SplitPart,
    RegexpMatch,
    HmacString,
    HmacBytes,
    ErrorIfNull,
}

impl VariadicFunc {
    pub fn eval<'a>(
        &'a self,
        datums: &[Datum<'a>],
        temp_storage: &'a RowArena,
        exprs: &'a [MirScalarExpr],
    ) -> Result<Datum<'a>, EvalError> {
        macro_rules! eager {
            ($func:ident $(, $args:expr)*) => {{
                let ds = exprs.iter()
                    .map(|e| e.eval(datums, temp_storage))
                    .collect::<Result<Vec<_>, _>>()?;
                if self.propagates_nulls() && ds.iter().any(|d| d.is_null()) {
                    return Ok(Datum::Null);
                }
                $func(&ds $(, $args)*)
            }}
        }

        match self {
            VariadicFunc::Coalesce => coalesce(datums, temp_storage, exprs),
            VariadicFunc::Concat => Ok(eager!(text_concat_variadic, temp_storage)),
            VariadicFunc::MakeTimestamp => Ok(eager!(make_timestamp)),
            VariadicFunc::PadLeading => eager!(pad_leading, temp_storage),
            VariadicFunc::Substr => eager!(substr),
            VariadicFunc::Replace => Ok(eager!(replace, temp_storage)),
            VariadicFunc::JsonbBuildArray => Ok(eager!(jsonb_build_array, temp_storage)),
            VariadicFunc::JsonbBuildObject => Ok(eager!(jsonb_build_object, temp_storage)),
            VariadicFunc::ArrayCreate {
                elem_type: ScalarType::Array(_),
            } => eager!(array_create_multidim, temp_storage),
            VariadicFunc::ArrayCreate { .. } => eager!(array_create_scalar, temp_storage),
            VariadicFunc::ArrayToString { elem_type } => {
                eager!(array_to_string, elem_type, temp_storage)
            }
            VariadicFunc::ListCreate { .. } | VariadicFunc::RecordCreate { .. } => {
                Ok(eager!(list_create, temp_storage))
            }
            VariadicFunc::ListSlice => Ok(eager!(list_slice, temp_storage)),
            VariadicFunc::SplitPart => eager!(split_part),
            VariadicFunc::RegexpMatch => eager!(regexp_match_dynamic, temp_storage),
            VariadicFunc::HmacString => eager!(hmac_string, temp_storage),
            VariadicFunc::HmacBytes => eager!(hmac_bytes, temp_storage),
            VariadicFunc::ErrorIfNull => error_if_null(datums, temp_storage, exprs),
        }
    }

    pub fn output_type(&self, input_types: Vec<ColumnType>) -> ColumnType {
        use VariadicFunc::*;
        match self {
            Coalesce => {
                assert!(input_types.len() > 0);
                debug_assert!(
                    input_types
                        .windows(2)
                        .all(|w| w[0].scalar_type.base_eq(&w[1].scalar_type)),
                    "coalesce inputs did not have uniform type: {:?}",
                    input_types
                );
                input_types.into_first().nullable(true)
            }
            Concat => ScalarType::String.nullable(true),
            MakeTimestamp => ScalarType::Timestamp.nullable(true),
            PadLeading => ScalarType::String.nullable(true),
            Substr => ScalarType::String.nullable(true),
            Replace => ScalarType::String.nullable(true),
            JsonbBuildArray | JsonbBuildObject => ScalarType::Jsonb.nullable(true),
            ArrayCreate { elem_type } => {
                debug_assert!(
                    input_types.iter().all(|t| t.scalar_type.base_eq(elem_type)),
                    "Args to ArrayCreate should have types that are compatible with the elem_type"
                );
                match elem_type {
                    ScalarType::Array(_) => elem_type.clone().nullable(false),
                    _ => ScalarType::Array(Box::new(elem_type.clone())).nullable(false),
                }
            }
            ArrayToString { .. } => ScalarType::String.nullable(true),
            ListCreate { elem_type } => {
                debug_assert!(
                    input_types.iter().all(|t| t.scalar_type.base_eq(elem_type)),
                    "Args to ListCreate should have types that are compatible with the elem_type"
                );
                ScalarType::List {
                    element_type: Box::new(elem_type.clone()),
                    custom_oid: None,
                }
                .nullable(false)
            }
            ListSlice { .. } => input_types[0].scalar_type.clone().nullable(true),
            RecordCreate { field_names } => ScalarType::Record {
                fields: field_names
                    .clone()
                    .into_iter()
                    .zip(input_types.into_iter())
                    .collect(),
                custom_oid: None,
                custom_name: None,
            }
            .nullable(true),
            SplitPart => ScalarType::String.nullable(true),
            RegexpMatch => ScalarType::Array(Box::new(ScalarType::String)).nullable(true),
            HmacString | HmacBytes => ScalarType::Bytes.nullable(true),
            ErrorIfNull => input_types[0].scalar_type.clone().nullable(false),
        }
    }

    /// Whether the function output is NULL if any of its inputs are NULL.
    pub fn propagates_nulls(&self) -> bool {
        !matches!(
            self,
            VariadicFunc::Coalesce
                | VariadicFunc::Concat
                | VariadicFunc::JsonbBuildArray
                | VariadicFunc::JsonbBuildObject
                | VariadicFunc::ListCreate { .. }
                | VariadicFunc::RecordCreate { .. }
                | VariadicFunc::ArrayCreate { .. }
                | VariadicFunc::ArrayToString { .. }
        )
    }
}

impl fmt::Display for VariadicFunc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            VariadicFunc::Coalesce => f.write_str("coalesce"),
            VariadicFunc::Concat => f.write_str("concat"),
            VariadicFunc::MakeTimestamp => f.write_str("makets"),
            VariadicFunc::PadLeading => f.write_str("lpad"),
            VariadicFunc::Substr => f.write_str("substr"),
            VariadicFunc::Replace => f.write_str("replace"),
            VariadicFunc::JsonbBuildArray => f.write_str("jsonb_build_array"),
            VariadicFunc::JsonbBuildObject => f.write_str("jsonb_build_object"),
            VariadicFunc::ArrayCreate { .. } => f.write_str("array_create"),
            VariadicFunc::ArrayToString { .. } => f.write_str("array_to_string"),
            VariadicFunc::ListCreate { .. } => f.write_str("list_create"),
            VariadicFunc::RecordCreate { .. } => f.write_str("record_create"),
            VariadicFunc::ListSlice => f.write_str("list_slice"),
            VariadicFunc::SplitPart => f.write_str("split_string"),
            VariadicFunc::RegexpMatch => f.write_str("regexp_match"),
            VariadicFunc::HmacString | VariadicFunc::HmacBytes => f.write_str("hmac"),
            VariadicFunc::ErrorIfNull => f.write_str("error_if_null"),
        }
    }
}

#[cfg(test)]
mod test {
    use chrono::prelude::*;

    use super::*;

    #[test]
    fn add_interval_months() {
        let dt = ym(2000, 1);

        assert_eq!(add_timestamp_months(dt, 0), dt);
        assert_eq!(add_timestamp_months(dt, 1), ym(2000, 2));
        assert_eq!(add_timestamp_months(dt, 12), ym(2001, 1));
        assert_eq!(add_timestamp_months(dt, 13), ym(2001, 2));
        assert_eq!(add_timestamp_months(dt, 24), ym(2002, 1));
        assert_eq!(add_timestamp_months(dt, 30), ym(2002, 7));

        // and negatives
        assert_eq!(add_timestamp_months(dt, -1), ym(1999, 12));
        assert_eq!(add_timestamp_months(dt, -12), ym(1999, 1));
        assert_eq!(add_timestamp_months(dt, -13), ym(1998, 12));
        assert_eq!(add_timestamp_months(dt, -24), ym(1998, 1));
        assert_eq!(add_timestamp_months(dt, -30), ym(1997, 7));

        // and going over a year boundary by less than a year
        let dt = ym(1999, 12);
        assert_eq!(add_timestamp_months(dt, 1), ym(2000, 1));
        let end_of_month_dt = NaiveDate::from_ymd(1999, 12, 31).and_hms(9, 9, 9);
        assert_eq!(
            // leap year
            add_timestamp_months(end_of_month_dt, 2),
            NaiveDate::from_ymd(2000, 2, 29).and_hms(9, 9, 9),
        );
        assert_eq!(
            // not leap year
            add_timestamp_months(end_of_month_dt, 14),
            NaiveDate::from_ymd(2001, 2, 28).and_hms(9, 9, 9),
        );
    }

    fn ym(year: i32, month: u32) -> NaiveDateTime {
        NaiveDate::from_ymd(year, month, 1).and_hms(9, 9, 9)
    }

    // Tests that `UnaryFunc::output_type` are consistent with
    // `UnaryFunc::introduces_nulls` and `UnaryFunc::propagates_nulls`.
    // Currently, only unit variants of UnaryFunc are tested because those are
    // the easiest to construct in bulk.
    #[test]
    fn unary_func_introduces_nulls() {
        // Dummy columns to test the nullability of `UnaryFunc::output_type`.
        // It is ok that we're feeding these dummy columns into functions that
        // may not even support this `ScalarType` as an input because we only
        // care about input and output nullabilities.
        let dummy_col_nullable_type = ScalarType::Bool.nullable(true);
        let dummy_col_nonnullable_type = ScalarType::Bool.nullable(false);
        for (variant, (_, f_types)) in UnaryFunc::mz_enum_reflect() {
            if f_types.is_empty() {
                let unary_unit_variant: UnaryFunc =
                    serde_json::from_str(&format!("\"{}\"", variant)).unwrap();
                let output_on_nullable_input = unary_unit_variant
                    .output_type(dummy_col_nullable_type.clone())
                    .nullable;
                let output_on_nonnullable_input = unary_unit_variant
                    .output_type(dummy_col_nonnullable_type.clone())
                    .nullable;
                if unary_unit_variant.introduces_nulls() {
                    // The output type should always be nullable no matter the
                    // input type.
                    assert!(output_on_nullable_input, "failure on {}", variant);
                    assert!(output_on_nonnullable_input, "failure on {}", variant)
                } else {
                    // The output type will be nonnullable if the input type is
                    // nonnullable. If the input type is nullable, the output
                    // type is equal to whether the func propagates nulls.
                    assert!(!output_on_nonnullable_input, "failure on {}", variant);
                    assert_eq!(
                        output_on_nullable_input,
                        unary_unit_variant.propagates_nulls()
                    );
                }
            }
        }
    }
}
