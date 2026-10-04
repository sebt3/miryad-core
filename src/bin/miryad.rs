//! Exécutable `miryad` livré avec le paquet de la crate — squelette de CLI de
//! scaffolding bâti sur `clap`. L'exécutable ne scaffold rien aujourd'hui : zéro
//! sous-commande, zéro option maison, zéro fichier lu ou écrit ; sa seule surface
//! observable est l'aide (`-h`, `--help`), la version (`-V`, `--version`) et le rejet
//! des arguments invalides, pour deux codes de sortie seulement : `0` (argv vide, aide,
//! version) et `2` (tout argv invalide). Le comportement « génère une application depuis
//! un modèle de données » porté par la ligne d'about dépend du tranchage du format du
//! modèle de données — question ouverte de la spec racine ; ce fichier fige l'état du
//! squelette, pas la promesse.

use clap::Parser;

/// CLI de scaffolding miryad — génère une application depuis un modèle de données.
#[derive(Parser)]
#[command(name = "miryad", version)]
struct Cli;

fn main() {
    Cli::parse();
}
