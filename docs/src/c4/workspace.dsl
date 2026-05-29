workspace "holospaces" "UOR-native boot layer over the hologram substrate" {

    model {
        operator = person "Operator" {
            description "Signs in with a self-sovereign identity and provisions, boots, and manages holospaces through the Platform Manager."
        }

        holospaces = softwareSystem "holospaces" {
            description "Boot layer: provisions and runs holospaces (bootable, content-addressed environments) and ships the Hologram Platform Manager."

            manager = container "Hologram Platform Manager" {
                description "The first-party holospace: the operator console that provisions and manages holospaces. Served from GitHub Pages."
            }
            bootLayer = container "Boot Layer" {
                description "Environment-agnostic core: composes the substrate pillars and the .holo engine; resolves a holospace and spawns it through the runtime."
            }
            holoEngine = container ".holo Engine" {
                description "A ContainerEngine backend that runs .holo compute artifacts via the hologram executor; native, and compiled to Wasm for the browser peer."
            }
            identity = container "Identity" {
                description "Self-sovereign sign-in key; links an operator's instances so their holospaces sync over the substrate."
            }
            realizations = container "Realizations" {
                description "holospaces' canonical-form types (e.g. the holospace), κ-addressed and verified by re-derivation."
            }
        }

        hologram = softwareSystem "hologram substrate" {
            description "External. Content-addressed compute (.holo), storage (KappaStore), network (KappaSync), and runtime (ContainerRuntime). Details: github.com/Hologram-Technologies/hologram."
            tags "External"
        }

        operator -> manager "Signs in; provisions and manages holospaces"
        manager -> bootLayer "Provisions / boots / suspends / resumes holospaces"
        bootLayer -> realizations "Resolves holospace definitions (κ)"
        bootLayer -> holoEngine "Runs .holo artifacts"
        bootLayer -> hologram "Stores/fetches content; runs containers; routes κ"
        holoEngine -> hologram "Executes via the hologram .holo executor"
        identity -> hologram "Syncs the operator's holospaces over KappaSync"
    }

    views {
        systemContext holospaces "c4-l1-system-context" {
            include *
            autoLayout
            description "Level 1: holospaces in context — the operator and the external hologram substrate."
        }
        container holospaces "c4-l2-holospaces-containers" {
            include *
            autoLayout
            description "Level 2: the holospaces containers and their relationship to the hologram substrate."
        }
        styles {
            element "Person" {
                shape person
            }
            element "External" {
                background "#999999"
                color "#ffffff"
            }
        }
    }
}
