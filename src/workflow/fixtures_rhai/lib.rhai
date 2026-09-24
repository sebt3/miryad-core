// Fixture de module Rhai pour le Scenario « default() n'ouvre aucun import, new(...) le
// permet » de ./../rhai_step.sdd — testée par ./../rhai_step.rs via
// `RhaiStep::new(vec![<ce répertoire>])`. Sous rhai 1.26.1 (lock de `vynil-core` 0.7.7), une
// `fn` de module est publique par défaut (`parser.rs` : `Token::Fn` → `FnAccess::Public`) —
// ni `pub fn` ni `export fn` n'est syntaxiquement valide pour une fonction.

fn value() {
    41
}
