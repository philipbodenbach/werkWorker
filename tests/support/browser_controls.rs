pub fn expanded_controls(value: &serde_json::Value) -> Vec<serde_json::Value> {
    use serde_json::{Value, json};
    if let Some(array) = value.as_array() {
        return array.clone();
    }
    assert_eq!(
        value["columns"],
        json!(["ref", "name", "role", "bounds", "other_attributes"])
    );
    assert_eq!(
        value["bounds_columns"],
        json!(["x", "y", "width", "height"])
    );
    value["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let mut control = value["defaults"].clone();
            control
                .as_object_mut()
                .unwrap()
                .extend(row[4].as_object().unwrap().clone());
            assert!(control.is_object());
            for (index, key) in ["ref", "name", "role"].iter().enumerate() {
                if row[index].is_string() {
                    control[key] = row[index].clone();
                }
            }
            let mut bounds = serde_json::Map::new();
            for (index, key) in ["x", "y", "width", "height"].iter().enumerate() {
                bounds.insert((*key).into(), row[3][index].clone());
            }
            control["bounds"] = Value::Object(bounds);
            control
        })
        .collect()
}
