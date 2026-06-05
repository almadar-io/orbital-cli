//! `orb new` — project scaffolding from embedded templates.

use crate::commands::compile::compile_schema;
use crate::{ServerFrameworkArg, ShellMode, ShellType};
use std::path::PathBuf;
use std::process::Command;

/// Template metadata
struct Template {
    name: &'static str,
    display_name: &'static str,
    description: &'static str,
    schema: &'static str,
    readme: &'static str,
}

/// Embedded templates
const TEMPLATES: &[Template] = &[
    Template {
        name: "blank",
        display_name: "Blank Project",
        description: "Minimal schema with a single orbital - start from scratch",
        schema: include_str!("../../../../templates/blank/schema.orb"),
        readme: include_str!("../../../../templates/blank/README.md"),
    },
    Template {
        name: "crud",
        display_name: "CRUD Application",
        description: "Full CRUD with list view, forms, and detail panels",
        schema: include_str!("../../../../templates/crud/schema.orb"),
        readme: include_str!("../../../../templates/crud/README.md"),
    },
    Template {
        name: "wizard",
        display_name: "Multi-Step Wizard",
        description: "Multi-step form flow with progress indicator",
        schema: include_str!("../../../../templates/wizard/schema.orb"),
        readme: include_str!("../../../../templates/wizard/README.md"),
    },
    Template {
        name: "dashboard",
        display_name: "Dashboard",
        description: "Multi-entity dashboard with tabs, stats, and charts",
        schema: include_str!("../../../../templates/dashboard/schema.orb"),
        readme: include_str!("../../../../templates/dashboard/README.md"),
    },
    Template {
        name: "saas",
        display_name: "SaaS Starter",
        description: "User onboarding, subscription plans, and billing",
        schema: include_str!("../../../../templates/saas/schema.orb"),
        readme: include_str!("../../../../templates/saas/README.md"),
    },
    Template {
        name: "game",
        display_name: "Game",
        description: "Game loop with canvas, player entity, and tick updates",
        schema: include_str!("../../../../templates/game/schema.orb"),
        readme: include_str!("../../../../templates/game/README.md"),
    },
    Template {
        name: "ai",
        display_name: "AI/ML Application",
        description: "Neural network training, model management, inference",
        schema: include_str!("../../../../templates/ai/schema.orb"),
        readme: include_str!("../../../../templates/ai/README.md"),
    },
    Template {
        name: "embedded",
        display_name: "Embedded/IoT",
        description: "Sensors, actuators, device lifecycle, power management",
        schema: include_str!("../../../../templates/embedded/schema.orb"),
        readme: include_str!("../../../../templates/embedded/README.md"),
    },
];

/// List available templates
pub(crate) fn list_templates() -> anyhow::Result<()> {
    println!("Available templates:\n");
    println!("{:<12} {:<25} {}", "NAME", "DISPLAY NAME", "DESCRIPTION");
    println!("{}", "=".chars().cycle().take(80).collect::<String>());

    for template in TEMPLATES {
        println!(
            "{:<12} {:<25} {}",
            template.name, template.display_name, template.description
        );
    }

    println!("\nUsage: orb new <project-name> --template <template-name>");
    println!("\nExample:");
    println!("  orb new my-app --template crud");

    Ok(())
}

/// Create a new project from a template
pub(crate) fn create_new_project(
    name: &str,
    template_name: &str,
    shell: ShellType,
    no_git: bool,
    no_install: bool,
) -> anyhow::Result<()> {
    use std::fs;

    // Find template
    let template = TEMPLATES
        .iter()
        .find(|t| t.name == template_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Template '{}' not found. Use --list to see available templates.",
                template_name
            )
        })?;

    println!("Creating new project: {}", name);
    println!("Template: {} ({})", template.display_name, template.name);
    println!();

    // Create project directory
    let project_dir = PathBuf::from(name);
    if project_dir.exists() {
        anyhow::bail!("Directory '{}' already exists", name);
    }
    fs::create_dir_all(&project_dir)?;

    // Process template variables
    let schema_content = template
        .schema
        .replace("{{projectName}}", name)
        .replace("{{entityName}}", "Item")
        .replace("{{wizardName}}", "Onboarding")
        .replace("{{entityName | lowercase}}", "item")
        .replace("{{wizardName | lowercase}}", "onboarding");

    let readme_content = template
        .readme
        .replace("{{projectName}}", name)
        .replace("{{entityName}}", "Item")
        .replace("{{wizardName}}", "Onboarding")
        .replace("{{entityName | lowercase}}", "item")
        .replace("{{wizardName | lowercase}}", "onboarding");

    // Write schema.orb
    let schema_path = project_dir.join("schema.orb");
    fs::write(&schema_path, &schema_content)?;
    println!("  Created schema.orb");

    // Write README.md
    let readme_path = project_dir.join("README.md");
    fs::write(&readme_path, &readme_content)?;
    println!("  Created README.md");

    // Compile the schema
    println!();
    println!("Compiling schema...");

    let app_dir = project_dir.join("app");
    match compile_schema(
        &schema_path,
        &app_dir,
        shell,
        ShellMode::Full,
        None,
        None,
        None,
        None,
        false,
        false,
        ServerFrameworkArg::Express,
        None,
    ) {
        Ok(()) => {}
        Err(e) => {
            // Shell template not found is a known distribution issue — give clear guidance
            let msg = e.to_string();
            if msg.contains("Shell not found") || msg.contains("not found") {
                eprintln!();
                eprintln!("Warning: Could not compile schema — shell template not found.");
                eprintln!("  Your schema.orb has been created successfully.");
                eprintln!();
                eprintln!("  To compile manually, install the shell template first:");
                eprintln!("    npm install -g @almadar/shell");
                eprintln!("  Then run:");
                eprintln!("    ORBITAL_SHELLS_DIR=$(npm root -g)/@almadar \\");
                eprintln!(
                    "      orb compile {}/schema.orb --output {}/app",
                    name, name
                );
                eprintln!();
                eprintln!("  Or set ORBITAL_SHELLS_DIR permanently in your shell profile.");
            } else {
                return Err(e);
            }
        }
    }

    // Initialize git
    if !no_git {
        println!();
        println!("Initializing git repository...");
        let status = Command::new("git")
            .args(["init"])
            .current_dir(&project_dir)
            .status();

        if status.is_ok() {
            // Create .gitignore
            let gitignore = "\
# Turbo\n\
.turbo/\n\
\n\
# Node\n\
node_modules/\n\
*.log\n\
\n\
# Build output\n\
dist/\n\
\n\
# Environment\n\
.env\n\
.env.local\n\
.env.*.local\n\
\n\
# Editor\n\
.vscode/\n\
.idea/\n\
*.swp\n\
*.swo\n\
\n\
# OS\n\
.DS_Store\n\
Thumbs.db\n\
\n\
# Test coverage\n\
coverage/\n\
.nyc_output/\n\
\n\
# Lock files (regenerated on install)\n\
package-lock.json\n";
            fs::write(project_dir.join(".gitignore"), gitignore)?;
            println!("  Initialized git repository");
        } else {
            println!("  Warning: Could not initialize git repository");
        }
    }

    // Install dependencies
    if !no_install {
        println!();
        match shell {
            ShellType::Typescript => {
                println!("Installing dependencies (npm install)...");
                let status = Command::new("npm")
                    .args(["install"])
                    .current_dir(&app_dir)
                    .status();

                if status.is_ok() {
                    println!("  Dependencies installed");
                } else {
                    println!(
                        "  Warning: Could not install dependencies. Run 'npm install' manually."
                    );
                }
            }
            ShellType::Python => {
                println!("To install dependencies, run:");
                println!("  cd {}/app", name);
                println!("  python -m venv .venv");
                println!("  source .venv/bin/activate");
                println!("  pip install -e .");
            }
            ShellType::Mobile => {
                println!("Installing dependencies (npm install)...");
                let status = Command::new("npm")
                    .args(["install"])
                    .current_dir(&app_dir)
                    .status();

                if status.is_ok() {
                    println!("  Dependencies installed");
                } else {
                    println!(
                        "  Warning: Could not install dependencies. Run 'npm install' manually."
                    );
                }
            }
        }
    }

    // Print next steps
    println!();
    println!("✅ Project created successfully!");
    println!();
    println!("Next steps:");
    println!("  cd {}", name);
    match shell {
        ShellType::Typescript => {
            if no_install {
                println!("  cd app && npm install");
            }
            println!("  cd app && npm run dev");
        }
        ShellType::Python => {
            println!("  cd app");
            println!("  python -m venv .venv && source .venv/bin/activate");
            println!("  pip install -e .");
            println!("  python -m orbital_app.main");
        }
        ShellType::Mobile => {
            if no_install {
                println!("  cd app && npm install");
            }
            println!("  cd app && npx expo start");
        }
    }
    println!();
    println!("To modify your schema, edit schema.orb and run:");
    println!("  orb compile schema.orb --output app");

    Ok(())
}
