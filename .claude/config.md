# Règles de développement

- TDD pour toutes les nouvelles implémentations
- Modifications atomiques : une tâche = un périmètre limité de fichiers
- Jamais modifier les sources avant que le plan de la tâche courante soit validé
- Pas de `Co-Authored-By` dans les messages de commit
- Les messages d'erreur disposent d'un identifiant unique

# Acteurs et rôles

Ce projet est développé à plusieurs :

- **Claude** : architecture, diagnostic complexe, prototypes d'interfaces, review du code produit
- **Qwen** (opencode, agents `implement` et `diagnose`) : implémentation guidée, tests, application de fixes précis
- **Les développeurs humains** : conception, debug, décisions techniques, coordination

# Workflow — mode feature

Une feature = un design d'architecture + des tâches just-in-time.

## Phase 1 — Design d'architecture (développeur + Claude)

Produit `docs/features/<feature>.md`, commité sur la branche feature. Contenu minimal :
- Ce que la feature fait (une phrase)
- Ce qu'elle ne fait pas (périmètre explicite)
- Les interfaces clés et les modules touchés
- Les risques identifiés et les questions ouvertes

Ce fichier est pour les développeurs et Claude — pas pour Qwen. Il est mis à jour si une découverte en cours d'implémentation change la compréhension.

Les développeurs et Claude ont chacun un droit de véto sur le passage à l'implémentation. "C'est prêt" requiert l'accord explicite des deux parties.

## Phase 2 — Implémentation (just-in-time)

- Une tâche à la fois. Jamais plus d'une ou deux d'avance.
- Chaque tâche est définie après la précédente, basée sur l'état réel du code et guidée par le design.
- La tâche produit un commit. La branche reste courte. On merge souvent.
- Qwen `implement` exécute, compile, teste, et remonte les blocages.

Règles :
- Le développeur principal décide quoi construire. Claude définit les interfaces et les scénarios de test. Qwen implémente.
- Une tâche = 30-45 minutes max ; si c'est plus long, on découpe
- Pas de passage à la tâche suivante avant que la précédente compile et que les tests passent
- Si Qwen remonte un blocage architectural, on met à jour le design avant d'écrire la prochaine tâche

## Phase 3 — Clôture (Claude)

Review du code produit. Une fois la feature validée :
- Les parties pertinentes du design migrent dans `docs/architecture.md`
- `docs/features/<feature>.md` est supprimé

# Workflow — mode debug / rattrapage

1. **Diagnostic** : Qwen `diagnose` pour les problèmes localisés, Claude pour les problèmes architecturaux
2. **Validation** (développeur + Claude) : on valide la cause et la solution choisie
3. **Fix** (Qwen `implement`) : reçoit le code cible ou un diff précis, applique, compile, teste

# Correction de bug

Quand un développeur décrit un bug ou donne un message d'erreur :

- Identifier le contexte : est-on sur une branche feature ? Le bug concerne-t-il la feature en cours ?
- Analyser : expliquer l'origine, proposer une ou des solutions
- Si le bug concerne la feature en cours : proposer un plan complémentaire, pas implémenter directement
- Sinon : proposer une branche `fix/description_courte` depuis origin/main, puis implémenter

Message de commit pour un fix :
```
fix: Description courte du bug

Explication brève du bug.
Explication courte du fix.
```

# Format des fichiers de tâche

Les fichiers de tâche sont produits par Claude et exécutés par Qwen (`implement`). Ils doivent être autonomes — Qwen n'a pas accès au contexte conversationnel.

Chemin : `.tasks/<feature>/task-XX-nom.md`
Les fichiers `.tasks/` ne sont jamais commités.

## Structure obligatoire

```
# Tâche XX — Nom

## Contexte
Feature concernée, position dans le flux global, dépendances sur les tâches précédentes.

## Fichiers à modifier
Chemins exacts depuis la racine. Pour chaque fichier existant : quelle fonction/section est modifiée.

## Code partiel
Interfaces, signatures de fonctions, squelettes.
Ces blocs sont des contrats — pas des suggestions. Signatures complètes.

## Tests
Fichier de test cible (chemin complet).
Cas de test : entrées, sorties ou comportements attendus.

## Commandes de validation
(Voir AGENTS.md — section "Commandes de validation")

## Commit
(feat: featureX) nom de tache

Synthèse de ce qui a été implémenté.
```

## Règles de rédaction

- Le fichier de tâche contient les interfaces (contrats entre modules) et les scénarios de test — pas les détails d'implémentation
- Si la tâche modifie une interface utilisée par d'autres modules, les lister et décrire l'adaptation requise
- Toutes les informations nécessaires sont dans le fichier de tâche — pas de "voir le design pour les détails"

# Directives générales

- Préférer modifier des fichiers existants plutôt qu'en créer de nouveaux
- Respecter `docs/architecture.md` s'il existe ; le mettre à jour si une feature modifie l'architecture globale
- Jamais `console.log` dans les sources — utiliser le logger du projet (voir `AGENTS.md`)
- Les fichiers `.tasks/` ne sont jamais commités
