//! YAML-based skill loader for `config/skills/*.yaml`.
//!
//! Skills defined in YAML files are merged with hardcoded skills at runtime.
//! Call [`invalidate_cache`] to trigger a reload on next [`load_yaml_skills`] call.

use std::sync::Mutex;

use crate::skills::SkillDefinition;

static CACHE: Mutex<Option<Vec<SkillDefinition>>> = Mutex::new(None);

/// Return all enabled skills from `config/skills/*.yaml`, using an in-memory cache.
pub fn load_yaml_skills() -> Vec<SkillDefinition> {
    let mut cache = CACHE.lock().unwrap();
    if let Some(ref cached) = *cache {
        return cached.clone();
    }
    let skills = scan_skills_dir();
    *cache = Some(skills.clone());
    skills
}

/// Clear the cache so the next call to [`load_yaml_skills`] re-reads disk.
pub fn invalidate_cache() {
    if let Ok(mut cache) = CACHE.lock() {
        *cache = None;
    }
}

fn scan_skills_dir() -> Vec<SkillDefinition> {
    let mut skills = Vec::new();
    let dir = std::path::Path::new("config/skills");
    if !dir.exists() {
        return skills;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[skill_loader] Cannot read config/skills/: {e}");
            return skills;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        match load_one(&path) {
            Ok(skill) if skill.enabled => skills.push(skill),
            Ok(_) => {}
            Err(e) => eprintln!("[skill_loader] Skipping {:?}: {e}", path.file_name().unwrap_or_default()),
        }
    }
    skills
}

fn load_one(path: &std::path::Path) -> Result<SkillDefinition, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    Ok(serde_yaml::from_str(&content)?)
}
