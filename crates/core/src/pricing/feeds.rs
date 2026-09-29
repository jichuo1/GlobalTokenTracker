//! Price-feed importers: models.dev, LiteLLM, llmpricing.dev.
//!
//! These used to download all three documents and parse each into a full
//! `serde_json::Value` tree — three multi-MB DOMs alive at once, a 50–80MB
//! heap spike on **every launch** (the startup price refresh). Now each feed is
//! parsed straight into a few typed fields (everything else is skipped without
//! being materialised) and written before the next one is fetched, so the peak
//! is a single document's worth of typed data.
//!
//! The feeds are third-party and their shapes drift, so parsing is *lenient*
//! exactly where the old `Value` code was: a number field of the wrong type is
//! "absent", a container of the wrong type is "empty" — never a parse error.
//! The `reference_*` functions in the tests are the original `Value`
//! implementations, and the equivalence tests pin the two to the same rows.

use super::normalize_key;
use anyhow::Result;
use rusqlite::Connection;
use serde::Deserialize;
use serde::de::value::MapAccessDeserializer;
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::fmt;
use std::marker::PhantomData;

// ------------------------------------------------------- lenient primitives

/// Drain whatever the deserializer is positioned on without keeping any of it.
fn skip_seq<'de, A: SeqAccess<'de>>(mut a: A) -> Result<(), A::Error> {
    while a.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

fn skip_map<'de, A: MapAccess<'de>>(mut a: A) -> Result<(), A::Error> {
    while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

/// A JSON number as `f64`; any other JSON value → `None` (`Value::as_f64`).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Num(Option<f64>);

impl<'de> Deserialize<'de> for Num {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Num;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_f64<E>(self, v: f64) -> Result<Num, E> {
                Ok(Num(Some(v)))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Num, E> {
                Ok(Num(Some(v as f64)))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Num, E> {
                Ok(Num(Some(v as f64)))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Num, E> {
                Ok(Num(None))
            }
            fn visit_str<E>(self, _: &str) -> Result<Num, E> {
                Ok(Num(None))
            }
            fn visit_unit<E>(self) -> Result<Num, E> {
                Ok(Num(None))
            }
            fn visit_none<E>(self) -> Result<Num, E> {
                Ok(Num(None))
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Num, D2::Error> {
                d.deserialize_any(self)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<Num, A::Error> {
                skip_seq(a).map(|()| Num(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Num, A::Error> {
                skip_map(a).map(|()| Num(None))
            }
        }
        d.deserialize_any(V)
    }
}

/// A JSON string; any other value → `None` (`Value::as_str`).
#[derive(Clone, Default, Debug, PartialEq)]
struct Text(Option<String>);

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Text;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E>(self, v: &str) -> Result<Text, E> {
                Ok(Text(Some(v.to_string())))
            }
            fn visit_string<E>(self, v: String) -> Result<Text, E> {
                Ok(Text(Some(v)))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Text, E> {
                Ok(Text(None))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Text, E> {
                Ok(Text(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Text, E> {
                Ok(Text(None))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Text, E> {
                Ok(Text(None))
            }
            fn visit_unit<E>(self) -> Result<Text, E> {
                Ok(Text(None))
            }
            fn visit_none<E>(self) -> Result<Text, E> {
                Ok(Text(None))
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Text, D2::Error> {
                d.deserialize_any(self)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<Text, A::Error> {
                skip_seq(a).map(|()| Text(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Text, A::Error> {
                skip_map(a).map(|()| Text(None))
            }
        }
        d.deserialize_any(V)
    }
}

/// `T` when the JSON value is an object; anything else → `None` — the typed
/// twin of indexing a `Value` that turns out not to be an object.
#[derive(Debug)]
struct Loose<T>(Option<T>);

impl<T> Default for Loose<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Loose<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Loose<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Loose<T>, A::Error> {
                T::deserialize(MapAccessDeserializer::new(a)).map(|t| Loose(Some(t)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<Loose<T>, A::Error> {
                skip_seq(a).map(|()| Loose(None))
            }
            fn visit_str<E>(self, _: &str) -> Result<Loose<T>, E> {
                Ok(Loose(None))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Loose<T>, E> {
                Ok(Loose(None))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Loose<T>, E> {
                Ok(Loose(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Loose<T>, E> {
                Ok(Loose(None))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Loose<T>, E> {
                Ok(Loose(None))
            }
            fn visit_unit<E>(self) -> Result<Loose<T>, E> {
                Ok(Loose(None))
            }
            fn visit_none<E>(self) -> Result<Loose<T>, E> {
                Ok(Loose(None))
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Loose<T>, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// The entries of a JSON object, **in document order** (the old code relied on
/// `serde_json`'s `preserve_order`: with `INSERT OR REPLACE` a later duplicate
/// key wins); anything that is not an object → no entries.
#[derive(Debug)]
struct Entries<T>(Vec<(String, T)>);

impl<T> Default for Entries<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Entries<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Entries<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Entries<T>, A::Error> {
                let mut out = Vec::new();
                while let Some(kv) = a.next_entry::<String, T>()? {
                    out.push(kv);
                }
                Ok(Entries(out))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<Entries<T>, A::Error> {
                skip_seq(a).map(|()| Entries(Vec::new()))
            }
            fn visit_str<E>(self, _: &str) -> Result<Entries<T>, E> {
                Ok(Entries(Vec::new()))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Entries<T>, E> {
                Ok(Entries(Vec::new()))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Entries<T>, E> {
                Ok(Entries(Vec::new()))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Entries<T>, E> {
                Ok(Entries(Vec::new()))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Entries<T>, E> {
                Ok(Entries(Vec::new()))
            }
            fn visit_unit<E>(self) -> Result<Entries<T>, E> {
                Ok(Entries(Vec::new()))
            }
            fn visit_none<E>(self) -> Result<Entries<T>, E> {
                Ok(Entries(Vec::new()))
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Entries<T>, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// The elements of a JSON array; anything that is not an array → none.
#[derive(Debug)]
struct Items<T>(Vec<T>);

impl<T> Default for Items<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Items<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Items<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Items<T>, A::Error> {
                let mut out = Vec::new();
                while let Some(x) = a.next_element::<T>()? {
                    out.push(x);
                }
                Ok(Items(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Items<T>, A::Error> {
                skip_map(a).map(|()| Items(Vec::new()))
            }
            fn visit_str<E>(self, _: &str) -> Result<Items<T>, E> {
                Ok(Items(Vec::new()))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Items<T>, E> {
                Ok(Items(Vec::new()))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Items<T>, E> {
                Ok(Items(Vec::new()))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Items<T>, E> {
                Ok(Items(Vec::new()))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Items<T>, E> {
                Ok(Items(Vec::new()))
            }
            fn visit_unit<E>(self) -> Result<Items<T>, E> {
                Ok(Items(Vec::new()))
            }
            fn visit_none<E>(self) -> Result<Items<T>, E> {
                Ok(Items(Vec::new()))
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Items<T>, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

// ------------------------------------------------------------ models.dev

/// `{provider: {models: {id: {cost: {input,output,cache_read,cache_write}}}}}`
#[derive(Deserialize, Default)]
struct MdProvider {
    #[serde(default)]
    models: Entries<Loose<MdModel>>,
}

#[derive(Deserialize, Default)]
struct MdModel {
    #[serde(default)]
    cost: Loose<MdCost>,
}

#[derive(Deserialize, Default)]
struct MdCost {
    #[serde(default)]
    input: Num,
    #[serde(default)]
    output: Num,
    #[serde(default)]
    cache_read: Num,
    #[serde(default)]
    cache_write: Num,
}

pub(super) fn import_models_dev(conn: &Connection, body: &str, now: i64) -> Result<usize> {
    let providers: Entries<Loose<MdProvider>> = serde_json::from_str(body)?;
    let mut st = conn.prepare(
        "INSERT OR REPLACE INTO prices(provider, model_id, input, output, cache_read, cache_write, source, fetched_at)
         VALUES ('models.dev', ?1, ?2, ?3, ?4, ?5, 'models.dev', ?6)",
    )?;
    let mut n = 0usize;
    for (_, prov) in providers.0 {
        let Some(prov) = prov.0 else { continue };
        for (id, model) in prov.models.0 {
            let Some(MdModel {
                cost: Loose(Some(c)),
            }) = model.0
            else {
                continue; // no cost object
            };
            st.execute(rusqlite::params![
                normalize_key(&id),
                c.input.0.unwrap_or(0.0),
                c.output.0.unwrap_or(0.0),
                c.cache_read.0.unwrap_or(0.0),
                c.cache_write.0.unwrap_or(0.0),
                now
            ])?;
            n += 1;
        }
    }
    Ok(n)
}

// --------------------------------------------------------------- LiteLLM

/// `{model: {input_cost_per_token, …}}` — all $/token, ×1e6 to $/1M.
#[derive(Deserialize, Default)]
struct LlEntry {
    #[serde(default)]
    input_cost_per_token: Num,
    #[serde(default)]
    output_cost_per_token: Num,
    #[serde(default)]
    cache_read_input_token_cost: Num,
    #[serde(default)]
    cache_creation_input_token_cost: Num,
    #[serde(default)]
    input_cost_per_token_above_200k_tokens: Num,
    #[serde(default)]
    cache_creation_input_token_cost_above_1hr: Num,
    #[serde(default)]
    input_cost_per_token_batches: Num,
}

pub(super) fn import_litellm(conn: &Connection, body: &str, now: i64) -> Result<usize> {
    let entries: Entries<Loose<LlEntry>> = serde_json::from_str(body)?;
    let mut st = conn.prepare(
        "INSERT OR REPLACE INTO prices(provider, model_id, input, output, cache_read, cache_write,
                tier_above_200k_input, tier_1h_cache_write, tier_batch, source, fetched_at)
         VALUES ('litellm', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'litellm', ?9)",
    )?;
    let per_1m = |n: Num| n.0.map(|x| x * 1e6);
    let mut n = 0usize;
    for (id, entry) in entries.0 {
        let Some(e) = entry.0 else { continue };
        if e.input_cost_per_token.0.is_none() {
            continue; // skip spec entries ("sample_spec", defaults)
        }
        st.execute(rusqlite::params![
            normalize_key(&id),
            per_1m(e.input_cost_per_token).unwrap_or(0.0),
            per_1m(e.output_cost_per_token).unwrap_or(0.0),
            per_1m(e.cache_read_input_token_cost).unwrap_or(0.0),
            per_1m(e.cache_creation_input_token_cost).unwrap_or(0.0),
            per_1m(e.input_cost_per_token_above_200k_tokens),
            per_1m(e.cache_creation_input_token_cost_above_1hr),
            per_1m(e.input_cost_per_token_batches),
            now
        ])?;
        n += 1;
    }
    Ok(n)
}

// ---------------------------------------------------------- llmpricing.dev

/// `{models:[{id, reference:{provider,input,output,cacheRead,official},
/// cheapest:{...}}]}` — already $/1M, no conversion. Books the official
/// `reference` quote; `cheapest` only fills gaps so a bargain host never
/// understates the user's actual provider cost. The source exposes no
/// cache-write price → the column stays NULL (honest absence, not a guessed
/// multiplier).
#[derive(Deserialize, Default)]
struct LpRoot {
    #[serde(default)]
    models: Items<Loose<LpModel>>,
}

#[derive(Deserialize, Default)]
struct LpModel {
    #[serde(default)]
    id: Text,
    #[serde(default)]
    reference: Loose<LpQuote>,
    #[serde(default)]
    cheapest: Loose<LpQuote>,
}

#[derive(Deserialize, Default)]
struct LpQuote {
    #[serde(default)]
    input: Num,
    #[serde(default)]
    output: Num,
    #[serde(default, rename = "cacheRead")]
    cache_read: Num,
}

pub(super) fn import_llmpricing(conn: &Connection, body: &str, now: i64) -> Result<usize> {
    let root: Loose<LpRoot> = serde_json::from_str(body)?;
    let mut st = conn.prepare(
        "INSERT OR REPLACE INTO prices(provider, model_id, input, output, cache_read, cache_write, source, fetched_at)
         VALUES ('llmpricing', ?1, ?2, ?3, ?4, NULL, 'llmpricing', ?5)",
    )?;
    let mut n = 0usize;
    for m in root.0.unwrap_or_default().models.0 {
        let Some(m) = m.0 else { continue };
        let Some(id) = m.id.0 else { continue };
        let reference = m.reference.0.unwrap_or_default();
        let q = if reference.input.0.is_some() {
            reference
        } else {
            m.cheapest.0.unwrap_or_default()
        };
        if q.input.0.is_none() && q.output.0.is_none() {
            continue; // neither quote usable
        }
        st.execute(rusqlite::params![
            normalize_key(&id),
            q.input.0.unwrap_or(0.0),
            q.output.0.unwrap_or(0.0),
            q.cache_read.0.unwrap_or(0.0),
            now
        ])?;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use serde_json::{Value, json};

    // -- the original Value-based importers, kept verbatim as the oracle ----

    fn reference_models_dev(conn: &Connection, md: &Value, now: i64) -> Result<usize> {
        let mut st = conn.prepare(
            "INSERT OR REPLACE INTO prices(provider, model_id, input, output, cache_read, cache_write, source, fetched_at)
             VALUES ('models.dev', ?1, ?2, ?3, ?4, ?5, 'models.dev', ?6)",
        )?;
        let mut n = 0;
        for prov in md.as_object().into_iter().flatten() {
            for (id, m) in prov.1["models"].as_object().into_iter().flatten() {
                let c = &m["cost"];
                if !c.is_object() {
                    continue;
                }
                let f = |k: &str| c[k].as_f64().unwrap_or(0.0);
                st.execute(rusqlite::params![
                    normalize_key(id),
                    f("input"),
                    f("output"),
                    f("cache_read"),
                    f("cache_write"),
                    now
                ])?;
                n += 1;
            }
        }
        Ok(n)
    }

    fn reference_litellm(conn: &Connection, ll: &Value, now: i64) -> Result<usize> {
        let mut st = conn.prepare(
            "INSERT OR REPLACE INTO prices(provider, model_id, input, output, cache_read, cache_write,
                    tier_above_200k_input, tier_1h_cache_write, tier_batch, source, fetched_at)
             VALUES ('litellm', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'litellm', ?9)",
        )?;
        let m = |v: &Value, k: &str| v[k].as_f64().map(|x| x * 1e6);
        let mut n = 0;
        for (id, v) in ll.as_object().into_iter().flatten() {
            if !v["input_cost_per_token"].is_number() {
                continue;
            }
            st.execute(rusqlite::params![
                normalize_key(id),
                m(v, "input_cost_per_token").unwrap_or(0.0),
                m(v, "output_cost_per_token").unwrap_or(0.0),
                m(v, "cache_read_input_token_cost").unwrap_or(0.0),
                m(v, "cache_creation_input_token_cost").unwrap_or(0.0),
                m(v, "input_cost_per_token_above_200k_tokens"),
                m(v, "cache_creation_input_token_cost_above_1hr"),
                m(v, "input_cost_per_token_batches"),
                now
            ])?;
            n += 1;
        }
        Ok(n)
    }

    fn reference_llmpricing(conn: &Connection, lp: &Value, now: i64) -> Result<usize> {
        let mut st = conn.prepare(
            "INSERT OR REPLACE INTO prices(provider, model_id, input, output, cache_read, cache_write, source, fetched_at)
             VALUES ('llmpricing', ?1, ?2, ?3, ?4, NULL, 'llmpricing', ?5)",
        )?;
        let mut n = 0usize;
        for m in lp["models"].as_array().into_iter().flatten() {
            let Some(id) = m["id"].as_str() else { continue };
            let q = if m["reference"]["input"].is_number() {
                &m["reference"]
            } else {
                &m["cheapest"]
            };
            if !q["input"].is_number() && !q["output"].is_number() {
                continue;
            }
            st.execute(rusqlite::params![
                normalize_key(id),
                q["input"].as_f64().unwrap_or(0.0),
                q["output"].as_f64().unwrap_or(0.0),
                q["cacheRead"].as_f64().unwrap_or(0.0),
                now
            ])?;
            n += 1;
        }
        Ok(n)
    }

    /// Every row of `prices`, in a comparable text form.
    fn dump(s: &Store) -> Vec<String> {
        let mut st = s
            .conn()
            .prepare(
                "SELECT provider||'|'||model_id||'|'||COALESCE(input,'N')||'|'||COALESCE(output,'N')||'|'||
                        COALESCE(cache_read,'N')||'|'||COALESCE(cache_write,'N')||'|'||
                        COALESCE(tier_above_200k_input,'N')||'|'||COALESCE(tier_1h_cache_write,'N')||'|'||
                        COALESCE(tier_batch,'N')||'|'||source||'|'||fetched_at
                 FROM prices WHERE source != 'seed' ORDER BY 1",
            )
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    /// Run `typed(body)` and `reference(value)` into two fresh stores and
    /// demand identical counts and identical `prices` rows.
    fn same_rows(
        doc: &Value,
        typed: fn(&Connection, &str, i64) -> Result<usize>,
        reference: fn(&Connection, &Value, i64) -> Result<usize>,
    ) {
        let (a, b) = (Store::open_memory().unwrap(), Store::open_memory().unwrap());
        let body = doc.to_string();
        let na = typed(a.conn(), &body, 42).unwrap();
        let nb = reference(b.conn(), doc, 42).unwrap();
        assert_eq!(na, nb, "row count");
        assert_eq!(dump(&a), dump(&b), "rows for {body}");
    }

    #[test]
    fn models_dev_matches_the_value_implementation() {
        same_rows(
            &json!({
                "openai": {"name": "OpenAI", "models": {
                    "gpt-5": {"cost": {"input": 1.25, "output": 10, "cache_read": 0.125}, "limit": {"context": 400000}, "junk": [1,2,{"a":null}]},
                    "GPT-5-Mini": {"cost": {"input": 0.25, "output": 2, "cache_read": "oops", "cache_write": null}},
                    "no-cost": {"name": "x"},
                    "cost-not-object": {"cost": 5},
                    "cost-array": {"cost": [1,2]},
                    "not-an-object-model": 7,
                    "openai/dup:latest": {"cost": {"input": 3, "output": 4}},
                    "dup": {"cost": {"input": 9, "output": 9}}
                }},
                "weird-provider": {"models": [1, 2, 3]},
                "no-models": {"name": "n"},
                "scalar-provider": 12,
                "null-provider": null,
                "anthropic": {"models": {"claude-x[1m]": {"cost": {"input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75, "extra": {"k": [1]}}}}}
            }),
            import_models_dev,
            reference_models_dev,
        );
        // top-level not an object → nothing, no error
        same_rows(&json!([1, 2]), import_models_dev, reference_models_dev);
        same_rows(&json!("text"), import_models_dev, reference_models_dev);
    }

    #[test]
    fn litellm_matches_the_value_implementation() {
        same_rows(
            &json!({
                "sample_spec": {"max_tokens": "set to max_output_tokens if provider specifies it"},
                "gpt-4o": {"input_cost_per_token": 2.5e-6, "output_cost_per_token": 1e-5, "cache_read_input_token_cost": 1.25e-6,
                           "litellm_provider": "openai", "supports_vision": true},
                "claude-x": {"input_cost_per_token": 3e-6, "output_cost_per_token": 15e-6, "cache_creation_input_token_cost": 3.75e-6,
                             "input_cost_per_token_above_200k_tokens": 6e-6, "cache_creation_input_token_cost_above_1hr": 6e-6,
                             "input_cost_per_token_batches": 1.5e-6},
                "int-cost": {"input_cost_per_token": 1, "output_cost_per_token": 2},
                "string-cost": {"input_cost_per_token": "0.000001"},
                "no-input": {"output_cost_per_token": 1e-6},
                "scalar-entry": 4,
                "array-entry": [1],
                "bedrock/anthropic.claude-y:0": {"input_cost_per_token": 1e-6},
                "claude-y": {"input_cost_per_token": 2e-6}
            }),
            import_litellm,
            reference_litellm,
        );
        same_rows(&json!(null), import_litellm, reference_litellm);
    }

    #[test]
    fn llmpricing_matches_the_value_implementation() {
        same_rows(
            &json!({"meta": {"models": 6}, "models": [
                {"id": "a/model-a", "reference": {"provider": "p", "input": 1, "output": 2, "cacheRead": 0.1, "official": true},
                 "cheapest": {"input": 0.5, "output": 1}},
                {"id": "model-b", "reference": {"provider": "p"}, "cheapest": {"input": 0.3, "output": 0.9, "cacheRead": 0.03}},
                {"id": "model-c", "reference": {"output": 5}, "cheapest": {"output": 4}},
                {"id": "model-d", "reference": null, "cheapest": {}},
                {"id": 12, "reference": {"input": 1}},
                {"reference": {"input": 1}},
                "junk", 5, null,
                {"id": "model-e", "reference": "str", "cheapest": [1]},
                {"id": "model-f", "reference": {"input": "1"}, "cheapest": {"input": 2, "output": 3}}
            ]}),
            import_llmpricing,
            reference_llmpricing,
        );
        same_rows(
            &json!({"models": {"a": 1}}),
            import_llmpricing,
            reference_llmpricing,
        );
        same_rows(&json!([1]), import_llmpricing, reference_llmpricing);
    }

    /// The real feeds, against the oracle. Needs network-downloaded files, so
    /// it only runs on request:
    /// `GTT_FEED_DIR=<dir with models_dev.json, litellm.json, llmpricing.json>
    ///  cargo test -p globaltokentracker-core real_feeds -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GTT_FEED_DIR with the downloaded feeds"]
    fn real_feeds_match_the_value_implementation() {
        let Some(dir) = std::env::var_os("GTT_FEED_DIR") else {
            eprintln!("GTT_FEED_DIR not set — skipped");
            return;
        };
        type Typed = fn(&Connection, &str, i64) -> Result<usize>;
        type Oracle = fn(&Connection, &Value, i64) -> Result<usize>;
        let cases: [(&str, Typed, Oracle); 3] = [
            ("models_dev.json", import_models_dev, reference_models_dev),
            ("litellm.json", import_litellm, reference_litellm),
            ("llmpricing.json", import_llmpricing, reference_llmpricing),
        ];
        for (file, typed, oracle) in cases {
            let body = std::fs::read_to_string(std::path::Path::new(&dir).join(file)).unwrap();
            let value: Value = serde_json::from_str(&body).unwrap();
            let (a, b) = (Store::open_memory().unwrap(), Store::open_memory().unwrap());
            let na = typed(a.conn(), &body, 7).unwrap();
            let nb = oracle(b.conn(), &value, 7).unwrap();
            assert_eq!(na, nb, "{file}: row count");
            assert_eq!(dump(&a), dump(&b), "{file}: rows differ");
            eprintln!("{file}: {na} rows, identical to the Value implementation");
        }
    }

    #[test]
    fn malformed_json_is_an_error_not_a_partial_import() {
        let s = Store::open_memory().unwrap();
        assert!(import_models_dev(s.conn(), "{\"a\": {\"models\": ", 1).is_err());
        assert!(import_litellm(s.conn(), "not json", 1).is_err());
        assert_eq!(dump(&s).len(), 0);
    }
}
