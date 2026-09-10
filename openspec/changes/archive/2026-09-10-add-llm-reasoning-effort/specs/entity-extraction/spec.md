## ADDED Requirements

### Requirement: NER prompt exhaustiveness directive

The NER system prompt SHALL include an exhaustiveness directive instructing
the model to extract every explicitly-named entity in the chunk, including
entities referenced only via a wiki-link (`[[...]]`) or a bare name. The
directive is present in both the workspace prompt override and the embedded
default prompt. Its purpose is to keep extraction complete when the model's
reasoning effort is lowered (a low effort was measured to drop
explicitly-named entities absent this directive).

#### Scenario: Directive present in the rendered prompt
- **WHEN** the NER system prompt is rendered for any domain
- **THEN** the rendered prompt contains the exhaustiveness directive (extract every explicitly-named entity, including wiki-link-only references)

#### Scenario: Fallback prompt carries the directive
- **WHEN** the workspace prompt override is absent and the embedded default is used
- **THEN** the embedded default prompt also contains the exhaustiveness directive
