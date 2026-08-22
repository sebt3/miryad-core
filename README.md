# miryad-core

Le moteur derrière **miryad**, un template d'application Rust/Vue "à la Debian" : vous décrivez
votre modèle de données, miryad-core fournit tout le reste — authentification, gestion des
utilisateurs et des groupes, API REST et GraphQL, serveur MCP pour les agents LLM, moteur de
workflow, et un frontend d'administration — sans que vous ayez à écrire ou maintenir cette
mécanique vous-même.

## À qui ça s'adresse

Aux développeurs qui construisent une application métier de type CRUD-avec-workflow (gestion de
ressources partagées entre utilisateurs, avec automatisations) et qui préfèrent partir d'un socle
éprouvé plutôt que de reconstruire l'authentification, les API et l'admin à chaque projet.

## Ce que vous obtenez

- Authentification OIDC transparente (compatible Authentik), sessions et tokens API pour les
  intégrations machine
- Gestion des utilisateurs et des groupes, avec un RBAC applicatif par entité (qui peut lire,
  qui peut écrire, qui est propriétaire)
- Une API REST et une API GraphQL générées automatiquement depuis votre modèle de données —
  aucune route ni resolver à écrire à la main
- Un serveur MCP exposant les mêmes ressources en tools CRUD, avec une sortie pensée pour les LLM
- Un moteur de workflow (DAG) intégré, avec support natif des scripts d'automatisation
- Un frontend Vue 3 responsive avec un espace d'administration, prêt à l'emploi

## Comment ça s'utilise

*(à compléter une fois le format du modèle de données et le CLI de scaffolding disponibles — cf.
`docs/roadmap.md`)*

## État du projet

En cours de construction. Voir `docs/roadmap.md` pour les grandes étapes et `docs/features/` pour
la feature en cours de design/implémentation.

## Licence

BSD 3-Clause.
