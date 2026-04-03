Help the user view and edit the Sirin persona configuration.

1. Read `config/persona.yaml` and display its current contents in a structured format
2. Explain each field's effect on runtime behavior:
   - `identity.name` — used in LLM prompt as persona name
   - `identity.professional_tone` — controls response style: brief/detailed/casual
   - `response_style.voice` — injected into prompt as persona voice description
   - `response_style.ack_prefix` — prefix used in fallback replies
   - `objectives` — used by follow-up worker to determine task priority
   - `roi_thresholds` — used by follow-up worker LLM prompt context
3. Ask the user what they want to change
4. Apply the changes to config/persona.yaml
5. Warn if `professional_tone` is set to a value other than `brief`, `detailed`, or `casual` — those are the only valid enum values in persona.rs

Note: persona.yaml is reloaded on every message by `Persona::load()`, so changes take effect immediately without restarting the app.
