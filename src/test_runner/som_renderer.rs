//! Set-of-Mark (SoM) visual labeling system for Sirin test runner.
//!
//! **Purpose**: Reduce vision LLM token cost by labeling interactive elements
//! with numbers instead of asking the LLM to infer coordinates.
//!
//! **Workflow**:
//! 1. Collect clickable AXTree nodes (e.g., buttons, inputs)
//! 2. Fetch bounding boxes via CDP `DOM.getBoxModel(backend_id)`
//! 3. Render numbered labels on the screenshot (e.g., "1", "2", "3")
//! 4. Return marked image + internal label→coordinate mapping
//! 5. Vision LLM receives: "图片已标记。若需点击，直接说『点击 5 号』"
//! 6. Parse LLM response: extract label ID (e.g., 5) → lookup coordinates → click
//!
//! **Expected benefit**: 30-40% vision token reduction on subsequent screenshots
//! (LLM goes from "find this button" to "click label 5").

use serde_json::Value;
use std::collections::HashMap;

/// Configuration for SoM rendering: colors, font sizing, etc.
#[derive(Debug, Clone)]
pub struct SoMConfig {
    /// Font size (in pixels) for label text
    pub font_size: u32,

    /// Label background: (R, G, B, A) — default: semi-transparent white
    pub label_bg_color: (u8, u8, u8, u8),

    /// Label text color: (R, G, B, A) — default: black
    pub label_text_color: (u8, u8, u8, u8),

    /// Minimum element size (in pixels) to label — skip tiny elements
    pub min_element_size: u32,

    /// Only label these roles (e.g., ["button", "textbox", "link"])
    /// Empty = label all except "text" and "image"
    pub target_roles: Vec<String>,
}

impl Default for SoMConfig {
    fn default() -> Self {
        Self {
            font_size: 14,
            label_bg_color: (255, 255, 255, 200), // semi-transparent white
            label_text_color: (0, 0, 0, 255),     // black
            min_element_size: 20,
            target_roles: vec![
                "button".into(),
                "textbox".into(),
                "text input".into(),
                "link".into(),
                "menuitem".into(),
                "checkbox".into(),
                "radio".into(),
                "combobox".into(),
            ],
        }
    }
}

/// Stores the mapping from label ID → (x, y) coordinates for later execution.
///
/// Used at execution time to convert LLM's "click 5" → actual pixel coords.
#[derive(Debug, Clone)]
pub struct SoMLabelMap {
    /// label_id (1-based) → (center_x, center_y)
    pub map: HashMap<u32, (f64, f64)>,
}

impl SoMLabelMap {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Register a label with its pixel coordinates.
    pub fn insert(&mut self, label_id: u32, x: f64, y: f64) {
        self.map.insert(label_id, (x, y));
    }

    /// Retrieve coordinates for a given label.
    pub fn get(&self, label_id: u32) -> Option<(f64, f64)> {
        self.map.get(&label_id).copied()
    }

    /// Count of registered labels.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Default for SoMLabelMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Renderer stub — full implementation would use image crate (image::RgbaImage,
/// imageproc::drawing) to actually draw on pixels. For now, this is the API.
pub struct SoMRenderer {
    config: SoMConfig,
}

impl SoMRenderer {
    pub fn new(config: SoMConfig) -> Self {
        Self { config }
    }

    pub fn with_defaults() -> Self {
        Self::new(SoMConfig::default())
    }

    /// Analyzes AXTree nodes and prepares SoM label map.
    ///
    /// In a full implementation, this would:
    /// 1. Filter nodes by role (keep buttons, inputs, links, etc.)
    /// 2. Call CDP `DOM.getBoxModel(backend_id)` for each node
    /// 3. Check element size (skip if < min_element_size)
    /// 4. Assign label IDs (1-based)
    /// 5. Return HashMap: label_id → (x, y)
    ///
    /// For now, returns a stub label map (empty).
    pub fn prepare_label_map(
        &self,
        ax_nodes: &[Value],
    ) -> Result<SoMLabelMap, String> {
        let mut label_map = SoMLabelMap::new();
        let mut label_id = 1u32;

        for node in ax_nodes {
            // Parse node role and name
            let role = node
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("");
            let _name = node
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let backend_id = node
                .get("backend_id")
                .and_then(Value::as_u64)
                .map(|id| id as u32);

            // Filter by role
            if !self.config.target_roles.is_empty()
                && !self.config.target_roles.contains(&role.to_string())
            {
                continue;
            }

            // Skip nodes without backend_id (can't click them)
            let Some(_bid) = backend_id else {
                continue;
            };

            // In a real implementation:
            // let box_model = cdp_client.get_box_model(bid).await?;
            // let width = (box_model.border[1].x - box_model.border[0].x).abs() as u32;
            // let height = (box_model.border[2].y - box_model.border[0].y).abs() as u32;
            // if width < self.config.min_element_size || height < self.config.min_element_size {
            //     continue;
            // }
            // let x = box_model.content[0].x;
            // let y = box_model.content[0].y;
            // label_map.insert(label_id, (x, y));

            // For now, stub: assign sequential label IDs (real coords would come from CDP)
            // TODO: integrate with CDP to fetch real box models
            label_map.insert(label_id, 0.0, 0.0); // placeholder coords
            label_id += 1;
        }

        Ok(label_map)
    }

    /// Render labels on a screenshot (stub).
    ///
    /// Full implementation would:
    /// 1. Load PNG from base64
    /// 2. Create RGBA buffer
    /// 3. For each (label_id, x, y) in label_map, draw a numbered circle/badge
    /// 4. Return marked image as base64
    ///
    /// For now, returns the input unchanged (no-op).
    pub fn render_labels(
        &self,
        _screenshot_base64: &str,
        _label_map: &SoMLabelMap,
    ) -> Result<String, String> {
        // TODO: implement actual image rendering
        // For MVP, return input unchanged
        Ok(_screenshot_base64.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_som_label_map_basic() {
        let mut map = SoMLabelMap::new();
        map.insert(1, 100.0, 200.0);
        map.insert(2, 300.0, 400.0);

        assert_eq!(map.get(1), Some((100.0, 200.0)));
        assert_eq!(map.get(2), Some((300.0, 400.0)));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn test_som_config_defaults() {
        let config = SoMConfig::default();
        assert_eq!(config.font_size, 14);
        assert!(!config.target_roles.is_empty());
    }

    #[test]
    fn test_som_renderer_creation() {
        let renderer = SoMRenderer::with_defaults();
        assert!(renderer.config.target_roles.contains(&"button".into()));
    }
}
