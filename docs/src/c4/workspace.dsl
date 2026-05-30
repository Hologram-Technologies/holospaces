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
                description "Runs .holo (tensor) compute artifacts via the hologram executor — a distinct compute path, NOT the runtime's ContainerEngine; native, and compiled to Wasm for the browser peer."
            }
            executionSurface = container "Execution Surface" {
                description "The κ-addressed Wasm code-module contract a holospace's code binds (the hologram host ABI + container ABI); validated and enforced, then booted by the substrate's ContainerRuntime."
            }
            systemEmulator = container "System Emulator" {
                description "The execution codemodule for a general operating system (ADR-009): a system emulator over the host ABI that computes an arbitrary OS image — disk as κ-addressed blocks, console/input/network as hologram channels, running state as a κ snapshot."
            }
            workspaceProjection = container "Workspace Projection" {
                description "The Codespaces/Gitpod projection: a browser editor, file tree, and terminal over a running holospace — reading its environment content by κ and publishing operator input as canonical events on its channels."
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
        operator -> workspaceProjection "Launches; edits files and runs terminal commands"
        manager -> bootLayer "Provisions / boots / suspends / resumes holospaces"
        manager -> workspaceProjection "Launches a workspace projection for a running holospace"
        bootLayer -> realizations "Resolves holospace definitions (κ)"
        bootLayer -> executionSurface "Validates a holospace's code; spawns it"
        bootLayer -> holoEngine "Runs .holo artifacts"
        bootLayer -> hologram "Stores/fetches content; runs containers; routes κ"
        executionSurface -> systemEmulator "Boots the OS-emulator codemodule for a general OS"
        systemEmulator -> hologram "Reads/writes the κ-addressed disk + channels via the host ABI"
        holoEngine -> hologram "Executes via the hologram .holo executor"
        workspaceProjection -> hologram "Reads environment content by κ; publishes input as canonical events"
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
