//! Following page tokens for list operations and merging the pages into one
//! result.
use std::num::NonZeroUsize;

use anyhow::Result;
use serde_json::{Value, json};

pub(super) const PAGE_SIZE: u32 = 100;
pub(super) const MAX_PAGES: usize = 1000;
const TOKEN_FIELDS: &[&str] = &["nextPageToken", "nextPageCursor", "nextCursor"];

/// How much to fetch: `limit` items in total, over at most `pages` requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Plan {
    pub(super) limit: Option<usize>,
    pub(super) pages: usize,
}

impl Plan {
    pub(super) fn new(limit: Option<u32>, pages: Option<NonZeroUsize>, all: bool) -> Self {
        let pages = match pages {
            Some(pages) => pages.get(),
            None if all || limit.is_some() => MAX_PAGES,
            None => 1,
        };
        Self {
            limit: limit.map(|limit| limit as usize),
            pages,
        }
    }

    fn page_size(self, collected: usize) -> Option<usize> {
        self.limit
            .map(|limit| limit.saturating_sub(collected).min(PAGE_SIZE as usize))
    }
}

/// The query names one paged operation uses for its page size and token.
pub(super) struct Pager<'a> {
    pub(super) size: &'a str,
    pub(super) token: &'a str,
    pub(super) plan: Plan,
}

pub(super) struct Pages {
    pub(super) body: Value,
    pub(super) more: bool,
}

impl Pager<'_> {
    /// Fetches pages until the plan is met, a page comes back empty, the API
    /// has no next page, or `until` accepts a page.
    pub(super) fn collect(
        &self,
        mut request: Value,
        mut fetch: impl FnMut(&Value) -> Result<Value>,
        until: impl Fn(&Value) -> bool,
    ) -> Result<Pages> {
        let mut merged: Option<Value> = None;
        let mut items = 0;
        let mut pages = 0;
        loop {
            if let Some(size) = self.plan.page_size(items) {
                request["query"][self.size] = json!(size);
            }
            let body = fetch(&request)?;
            pages += 1;
            let token = next_token(&body);
            let added = item_count(&body);
            let done = until(&body);
            items += added;
            match merged.as_mut() {
                Some(merged) => append(merged, body),
                None => merged = Some(body),
            }
            let finished = done
                || added == 0
                || pages >= self.plan.pages
                || self.plan.limit.is_some_and(|limit| items >= limit);
            match token {
                Some(token) if !finished => {
                    request["query"][self.token] = Value::String(token);
                }
                token => return Ok(finish(merged.unwrap_or(Value::Null), token)),
            }
        }
    }
}

fn next_token(body: &Value) -> Option<String> {
    if body.get("hasMore").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    TOKEN_FIELDS
        .iter()
        .find_map(|field| body.get(*field)?.as_str())
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn item_count(body: &Value) -> usize {
    body.as_object().map_or(0, |object| {
        object
            .values()
            .filter_map(Value::as_array)
            .map(Vec::len)
            .sum()
    })
}

fn append(merged: &mut Value, page: Value) {
    let (Some(merged), Value::Object(page)) = (merged.as_object_mut(), page) else {
        return;
    };
    for (name, value) in page {
        if let (Some(Value::Array(items)), Value::Array(more)) = (merged.get_mut(&name), value) {
            items.extend(more);
        }
    }
}

fn finish(mut body: Value, token: Option<String>) -> Pages {
    let more = token.is_some();
    if let Some(object) = body.as_object_mut() {
        if let Some(field) = TOKEN_FIELDS
            .iter()
            .find(|field| object.contains_key(**field))
        {
            object.insert(
                (*field).to_string(),
                token.map_or(Value::Null, Value::String),
            );
        }
        if object.contains_key("hasMore") {
            object.insert("hasMore".to_string(), Value::Bool(more));
        }
        if more {
            object.insert("more".to_string(), Value::Bool(true));
        }
    }
    Pages { body, more }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn pages(count: usize, per_page: usize) -> Vec<Value> {
        (0..count)
            .map(|page| {
                json!({
                    "gameServerLogs": (0..per_page)
                        .map(|item| json!({ "message": format!("{page}-{item}") }))
                        .collect::<Vec<_>>(),
                    "nextPageToken": if page + 1 < count { Value::String(format!("t{}", page + 1)) } else { Value::Null },
                })
            })
            .collect()
    }

    fn run(plan: Plan, source: &[Value]) -> (Pages, Vec<Value>) {
        let requests = RefCell::new(Vec::new());
        let pager = Pager {
            size: "MaxPageSize",
            token: "PageToken",
            plan,
        };
        let result = pager
            .collect(
                json!({ "query": {} }),
                |request| {
                    let index = request["query"]["PageToken"]
                        .as_str()
                        .map_or(0, |token| token[1..].parse::<usize>().unwrap());
                    requests.borrow_mut().push(request["query"].clone());
                    let mut page = source[index].clone();
                    if let (Some(size), Some(items)) = (
                        request["query"]["MaxPageSize"].as_u64(),
                        page["gameServerLogs"].as_array_mut(),
                    ) {
                        items.truncate(size as usize);
                    }
                    Ok(page)
                },
                |_| false,
            )
            .unwrap();
        (result, requests.into_inner())
    }

    #[test]
    fn a_limit_is_a_total_fetched_one_hundred_at_a_time() {
        let source = pages(5, 100);
        let (result, requests) = run(Plan::new(Some(250), None, false), &source);
        assert_eq!(
            requests,
            vec![
                json!({"MaxPageSize": 100}),
                json!({"MaxPageSize": 100, "PageToken": "t1"}),
                json!({"MaxPageSize": 50, "PageToken": "t2"}),
            ]
        );
        assert_eq!(result.body["gameServerLogs"].as_array().unwrap().len(), 250);
        assert_eq!(result.body["gameServerLogs"][249]["message"], "2-49");
        assert_eq!(result.body["nextPageToken"], "t3");
        assert_eq!(result.body["more"], true);
        assert!(result.more);
    }

    #[test]
    fn the_last_page_clears_the_token_and_more() {
        let source = pages(3, 10);
        let (result, requests) = run(Plan::new(None, None, true), &source);
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0], json!({}));
        assert_eq!(result.body["gameServerLogs"].as_array().unwrap().len(), 30);
        assert_eq!(result.body["nextPageToken"], Value::Null);
        assert!(result.body.get("more").is_none());
        assert!(!result.more);
    }

    #[test]
    fn without_paging_flags_one_page_is_read() {
        let source = pages(3, 10);
        let (result, requests) = run(Plan::new(None, None, false), &source);
        assert_eq!(requests.len(), 1);
        assert_eq!(result.body["nextPageToken"], "t1");
        assert_eq!(result.body["more"], true);
        let (_, requests) = run(Plan::new(None, NonZeroUsize::new(2), false), &source);
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn an_empty_page_with_a_fresh_token_stops_the_loop() {
        let source = vec![
            json!({"items": [], "nextPageToken": "t1"}),
            json!({"items": [1], "nextPageToken": null}),
        ];
        let (result, requests) = run(Plan::new(None, None, true), &source);
        assert_eq!(requests.len(), 1);
        assert!(result.more);
    }

    #[test]
    fn has_more_false_ends_cursor_paging() {
        assert_eq!(
            next_token(&json!({"hasMore": false, "nextCursor": "abc"})),
            None
        );
        assert_eq!(
            next_token(&json!({"hasMore": true, "nextCursor": "abc"})),
            Some("abc".to_string())
        );
        assert_eq!(next_token(&json!({"nextPageCursor": ""})), None);
    }
}
