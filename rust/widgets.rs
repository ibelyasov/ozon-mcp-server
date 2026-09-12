use serde_json::Value;

pub struct WidgetSet<'a> {
    page: &'a Value,
}

impl<'a> WidgetSet<'a> {
    pub fn new(page: &'a Value) -> Self {
        Self { page }
    }

    pub fn first_matching(
        &self,
        name: &str,
        mut predicate: impl FnMut(&Value) -> bool,
    ) -> Option<Value> {
        self.all_valid(name).find(|value| predicate(value))
    }

    pub fn has_matching(&self, name: &str, predicate: impl FnMut(&Value) -> bool) -> bool {
        self.first_matching(name, predicate).is_some()
    }

    pub fn first_valid_where(&self, mut predicate: impl FnMut(&Value) -> bool) -> Option<Value> {
        self.page
            .get("widgetStates")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|states| states.values())
            .filter_map(decode)
            .find(|value| value.is_object() && predicate(value))
    }

    pub fn all_valid<'b>(&'b self, name: &'b str) -> impl Iterator<Item = Value> + 'b {
        self.page
            .get("widgetStates")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|states| states.iter())
            .filter(move |(key, _)| key.split('-').next() == Some(name))
            .filter_map(|(_, value)| decode(value))
            .filter(Value::is_object)
    }
}

fn decode(value: &Value) -> Option<Value> {
    if value.is_object() || value.is_array() {
        Some(value.clone())
    } else {
        serde_json::from_str(value.as_str()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn skips_malformed_instance_before_valid_instance() {
        let page = json!({"widgetStates": {
            "target-a": "{bad",
            "target-b": "{\"value\": 2}",
            "target-c": {"value": 3}
        }});
        let widgets = WidgetSet::new(&page);
        assert_eq!(
            widgets.first_matching("target", |_| true),
            Some(json!({"value": 2}))
        );
        assert_eq!(
            widgets.first_matching("target", |value| value["value"] == 3),
            Some(json!({"value": 3}))
        );
        assert_eq!(widgets.all_valid("target").count(), 2);
        assert_eq!(
            widgets.first_valid_where(|value| value["value"] == 3),
            Some(json!({"value": 3}))
        );
        assert!(!widgets.has_matching("missing", |_| true));
    }
}
