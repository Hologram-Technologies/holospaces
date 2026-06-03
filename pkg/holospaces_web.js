/* @ts-self-types="./holospaces_web.d.ts" */

/**
 * The Platform Manager console, running as a browser peer that composes the
 * substrate runtime over the interpreter `ContainerEngine`.
 */
export class Console {
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        ConsoleFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_console_free(ptr, 0);
    }
    /**
     * Boot a userland holospace **in the browser**: provision it, then spawn it
     * through the substrate runtime over the interpreter `ContainerEngine`,
     * capture a κ snapshot of its state (suspend), resume, and terminate — the
     * execution surface running on the browser peer (ADR-008; RT2; `CC-6`).
     * Returns the κ-label of the suspend snapshot (state is content, Law L3).
     * @param {Uint8Array} module
     * @param {number} memory_bytes
     * @returns {string}
     */
    boot_userland(module, memory_bytes) {
        let deferred3_0;
        let deferred3_1;
        try {
            const ptr0 = passArray8ToWasm0(module, wasm.__wbindgen_malloc);
            const len0 = WASM_VECTOR_LEN;
            const ret = wasm.console_boot_userland(this.__wbg_ptr, ptr0, len0, memory_bytes);
            var ptr2 = ret[0];
            var len2 = ret[1];
            if (ret[3]) {
                ptr2 = 0; len2 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred3_0 = ptr2;
            deferred3_1 = len2;
            return getStringFromWasm0(ptr2, len2);
        } finally {
            wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
        }
    }
    /**
     * *Control panel: configure.* Reconfigure a running instance from the panel
     * (ADR-018; `CC-28`). `directives_json` is a JSON array of operations across
     * the four classes, e.g. `[{"lifecycle":"suspend"}, {"forwardPort":8080},
     * {"unforwardPort":8080}, {"network":{"fetch":true,"announce":false}},
     * {"quota":1073741824}, {"grant":"blake3:…"}]`. The panel builds a
     * content-addressed [`Configuration`] issued by the signed-in operator,
     * stores it (Law L2), and returns its κ — the content the running instance
     * resolves and applies over the substrate (no server, no RPC).
     * @param {string} instance
     * @param {string} directives_json
     * @returns {string}
     */
    configure(instance, directives_json) {
        let deferred4_0;
        let deferred4_1;
        try {
            const ptr0 = passStringToWasm0(instance, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(directives_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len1 = WASM_VECTOR_LEN;
            const ret = wasm.console_configure(this.__wbg_ptr, ptr0, len0, ptr1, len1);
            var ptr3 = ret[0];
            var len3 = ret[1];
            if (ret[3]) {
                ptr3 = 0; len3 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred4_0 = ptr3;
            deferred4_1 = len3;
            return getStringFromWasm0(ptr3, len3);
        } finally {
            wasm.__wbindgen_free(deferred4_0, deferred4_1, 1);
        }
    }
    /**
     * Open a fresh console — a browser peer with a local content-addressed
     * store and the interpreter container engine.
     */
    constructor() {
        const ret = wasm.console_new();
        this.__wbg_ptr = ret;
        ConsoleFinalization.register(this, this.__wbg_ptr, this);
        return this;
    }
    /**
     * Provision a holospace from a `.holo` compute artifact (the *holo-file*
     * compute form) with a memory budget, κ-addressing its parts into the
     * peer's store (Law L2). Returns the holospace identity κ.
     * @param {Uint8Array} code
     * @param {number} memory_bytes
     * @returns {string}
     */
    provision(code, memory_bytes) {
        let deferred3_0;
        let deferred3_1;
        try {
            const ptr0 = passArray8ToWasm0(code, wasm.__wbindgen_malloc);
            const len0 = WASM_VECTOR_LEN;
            const ret = wasm.console_provision(this.__wbg_ptr, ptr0, len0, memory_bytes);
            var ptr2 = ret[0];
            var len2 = ret[1];
            if (ret[3]) {
                ptr2 = 0; len2 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred3_0 = ptr2;
            deferred3_1 = len2;
            return getStringFromWasm0(ptr2, len2);
        } finally {
            wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
        }
    }
    /**
     * Provision a holospace from a **devcontainer** for the management console
     * (CC-12): the `devcontainer.json` is validated against the Dev Container
     * spec (`CC-4`) and κ-addressed into the store; the holospace's identity is
     * the content address of its devcontainer definition (reproducible — same
     * source ⇒ same κ, Law L1). This *provisions* (records) the holospace; the
     * operator *enters* it to boot its OS in the workspace IDE (`CC-13`).
     * Returns the holospace identity κ.
     * @param {Uint8Array} config_json
     * @param {number} memory_bytes
     * @returns {string}
     */
    provision_devcontainer(config_json, memory_bytes) {
        let deferred3_0;
        let deferred3_1;
        try {
            const ptr0 = passArray8ToWasm0(config_json, wasm.__wbindgen_malloc);
            const len0 = WASM_VECTOR_LEN;
            const ret = wasm.console_provision_devcontainer(this.__wbg_ptr, ptr0, len0, memory_bytes);
            var ptr2 = ret[0];
            var len2 = ret[1];
            if (ret[3]) {
                ptr2 = 0; len2 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred3_0 = ptr2;
            deferred3_1 = len2;
            return getStringFromWasm0(ptr2, len2);
        } finally {
            wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
        }
    }
    /**
     * Provision a holospace from a *Wasm-recompiled userland* (the execution
     * surface, the second compute form — ADR-008). The module is validated
     * against the surface contract ([`validate_userland`]) before it is
     * κ-addressed into the store, so only a substrate-valid userland can become
     * a holospace's code. Returns the holospace identity κ.
     * @param {Uint8Array} module
     * @param {number} memory_bytes
     * @returns {string}
     */
    provision_userland(module, memory_bytes) {
        let deferred3_0;
        let deferred3_1;
        try {
            const ptr0 = passArray8ToWasm0(module, wasm.__wbindgen_malloc);
            const len0 = WASM_VECTOR_LEN;
            const ret = wasm.console_provision_userland(this.__wbg_ptr, ptr0, len0, memory_bytes);
            var ptr2 = ret[0];
            var len2 = ret[1];
            if (ret[3]) {
                ptr2 = 0; len2 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred3_0 = ptr2;
            deferred3_1 = len2;
            return getStringFromWasm0(ptr2, len2);
        } finally {
            wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
        }
    }
    /**
     * Resolve a holospace (or any κ) from this peer's own in-session store.
     * Returns the bytes, or `undefined` if absent.
     *
     * This is a *trusted* read ([`ReadVerify::Trusted`], ADR-019): the store is
     * the canonical memory and RAM is its cache (Law L3), so content that
     * entered this session was already verified on the way in (on receipt, or
     * by `put` construction). The deployed peer does not re-derive κ on every
     * local read — that would treat its own canonical store as untrusted and is
     * pure overhead. The re-derivation invariant still holds where untrusted
     * bytes enter (the import/fetch boundary) and is exercised end-to-end in CI.
     * @param {string} kappa
     * @returns {Uint8Array | undefined}
     */
    resolve(kappa) {
        const ptr0 = passStringToWasm0(kappa, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.console_resolve(this.__wbg_ptr, ptr0, len0);
        if (ret[3]) {
            throw takeFromExternrefTable0(ret[2]);
        }
        let v2;
        if (ret[0] !== 0) {
            v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
            wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        }
        return v2;
    }
    /**
     * The operator's roster κ — the content address that links their instances
     * (R5). Its bytes are in the store, so another instance can resolve it.
     * @returns {string | undefined}
     */
    roster_kappa() {
        const ret = wasm.console_roster_kappa(this.__wbg_ptr);
        let v1;
        if (ret[0] !== 0) {
            v1 = getStringFromWasm0(ret[0], ret[1]).slice();
            wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        }
        return v1;
    }
    /**
     * Import and run a **devcontainer in the browser** — the Codespaces/Gitpod
     * scenario without a Docker daemon or a cloud VM (arc42 chapter 1, the
     * motivating scenario; chapter 6). The `devcontainer.json` is validated
     * against the Dev Container spec (`CC-4`); the κ-addressed Wasm `userland`
     * its config selects is validated against the host-ABI surface (`CC-6`) and
     * booted through the substrate runtime over the interpreter engine — same
     * lifecycle as a native or remote peer (Q6). Returns the suspend snapshot κ.
     * @param {string} repo
     * @param {string} reference
     * @param {string} config_path
     * @param {Uint8Array} config_json
     * @param {Uint8Array} userland_module
     * @param {number} memory_bytes
     * @returns {string}
     */
    run_devcontainer(repo, reference, config_path, config_json, userland_module, memory_bytes) {
        let deferred7_0;
        let deferred7_1;
        try {
            const ptr0 = passStringToWasm0(repo, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(reference, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len1 = WASM_VECTOR_LEN;
            const ptr2 = passStringToWasm0(config_path, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len2 = WASM_VECTOR_LEN;
            const ptr3 = passArray8ToWasm0(config_json, wasm.__wbindgen_malloc);
            const len3 = WASM_VECTOR_LEN;
            const ptr4 = passArray8ToWasm0(userland_module, wasm.__wbindgen_malloc);
            const len4 = WASM_VECTOR_LEN;
            const ret = wasm.console_run_devcontainer(this.__wbg_ptr, ptr0, len0, ptr1, len1, ptr2, len2, ptr3, len3, ptr4, len4, memory_bytes);
            var ptr6 = ret[0];
            var len6 = ret[1];
            if (ret[3]) {
                ptr6 = 0; len6 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred7_0 = ptr6;
            deferred7_1 = len6;
            return getStringFromWasm0(ptr6, len6);
        } finally {
            wasm.__wbindgen_free(deferred7_0, deferred7_1, 1);
        }
    }
    /**
     * Sign in by unlocking a self-sovereign key (not a server account,
     * ADR-001). Returns the operator's content-addressed identity κ.
     * @param {Uint8Array} key
     * @returns {string}
     */
    sign_in(key) {
        let deferred2_0;
        let deferred2_1;
        try {
            const ptr0 = passArray8ToWasm0(key, wasm.__wbindgen_malloc);
            const len0 = WASM_VECTOR_LEN;
            const ret = wasm.console_sign_in(this.__wbg_ptr, ptr0, len0);
            deferred2_0 = ret[0];
            deferred2_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred2_0, deferred2_1, 1);
        }
    }
    /**
     * The console's View — a JSON projection of the operator and their
     * holospaces (what the UI renders).
     * @returns {string}
     */
    view() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.console_view(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
}
if (Symbol.dispose) Console.prototype[Symbol.dispose] = Console.prototype.free;

/**
 * A devcontainer's OCI image, assembled into a bootable root filesystem *in the
 * browser* — the Layer Assembler (`CC-7` / the in-crate ext4 writer) running as
 * the wasm peer. The operator's page fetches the devcontainer's image layers
 * from the cold-start gateway (verified by re-derivation before they are added),
 * then assembles them here; the result boots over the emulator's `virtio-blk`
 * ([`Workspace::boot_devcontainer`], `CC-14`). The browser peer *is* the
 * machine — no server assembles or boots the OS (Law L1/L4).
 */
export class DevcontainerImage {
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        DevcontainerImageFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_devcontainerimage_free(ptr, 0);
    }
    /**
     * Add an OCI image layer (its media type + the verified blob bytes), in
     * order from the base layer up.
     * @param {string} media_type
     * @param {Uint8Array} blob
     */
    add_layer(media_type, blob) {
        const ptr0 = passStringToWasm0(media_type, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passArray8ToWasm0(blob, wasm.__wbindgen_malloc);
        const len1 = WASM_VECTOR_LEN;
        wasm.devcontainerimage_add_layer(this.__wbg_ptr, ptr0, len0, ptr1, len1);
    }
    /**
     * Assemble the layers into a bootable `ext4` root filesystem (gunzip +
     * untar + OCI whiteout overlay + the in-crate ext4 writer; Law L4). The
     * bytes back a [`Workspace::boot_devcontainer`] machine's `virtio-blk` disk.
     * @returns {Uint8Array}
     */
    assemble() {
        const ret = wasm.devcontainerimage_assemble(this.__wbg_ptr);
        if (ret[3]) {
            throw takeFromExternrefTable0(ret[2]);
        }
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * Assemble the layers into a **bootable, interactive, writable** root
     * filesystem on a `disk_bytes`-sized disk: the same overlay as
     * [`Self::assemble`], plus the persistent devcontainer
     * [`/init`](holospaces::machine::DEVCONTAINER_INIT) injected — it mounts the
     * pseudo filesystems and the shared `virtio-9p` workspace and execs a shell,
     * so the booted OS stays running as a dev environment instead of powering off
     * after boot — and sized to `disk_bytes` so the OS has room to work (the
     * devcontainer's disk; the caller's to choose, not a hidden cap). The base
     * image must provide a static `/bin/busybox`.
     * @param {number} disk_bytes
     * @returns {Uint8Array}
     */
    assemble_bootable(disk_bytes) {
        const ret = wasm.devcontainerimage_assemble_bootable(this.__wbg_ptr, disk_bytes);
        if (ret[3]) {
            throw takeFromExternrefTable0(ret[2]);
        }
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * A new, empty image (add its layers lowest-first with [`Self::add_layer`]).
     */
    constructor() {
        const ret = wasm.devcontainerimage_new();
        this.__wbg_ptr = ret;
        DevcontainerImageFinalization.register(this, this.__wbg_ptr, this);
        return this;
    }
}
if (Symbol.dispose) DevcontainerImage.prototype[Symbol.dispose] = DevcontainerImage.prototype.free;

/**
 * A **workspace** over a running holospace, in the browser tab — the
 * Codespaces/Gitpod experience (ADR-009; `CC-9` + `CC-11`). The operator
 * launches a holospace whose code is the system emulator; it **boots a real
 * operating system** (the [system emulator](holospaces::emulator) running in
 * the browser's own wasm engine), and the [workspace
 * projection](holospaces::projection) drives it: a live **terminal**
 * (keystrokes published as canonical events that advance the holospace's κ
 * snapshot) and an **editor** that reads and edits environment content *by κ*.
 *
 * The boot runs in instruction *chunks* ([`run`](Workspace::run)) so the UI
 * stays responsive and can stream the console as the kernel boots — there is no
 * server doing the work; the browser peer *is* the machine (Law L1).
 */
export class Workspace {
    static __wrap(ptr) {
        const obj = Object.create(Workspace.prototype);
        obj.__wbg_ptr = ptr;
        WorkspaceFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        WorkspaceFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_workspace_free(ptr, 0);
    }
    /**
     * Launch a workspace: place the OS `kernel` image and `dtb` in a machine
     * with `ram_bytes` of RAM at `base`, the device tree at `dtb_addr`, and hand
     * off as the SBI firmware. The machine is now booting (drive it with
     * [`run`](Workspace::run)).
     * @param {Uint8Array} kernel
     * @param {Uint8Array} dtb
     * @param {number} ram_bytes
     * @param {number} base
     * @param {number} dtb_addr
     * @returns {Workspace}
     */
    static boot(kernel, dtb, ram_bytes, base, dtb_addr) {
        const ptr0 = passArray8ToWasm0(kernel, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passArray8ToWasm0(dtb, wasm.__wbindgen_malloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.workspace_boot(ptr0, len0, ptr1, len1, ram_bytes, base, dtb_addr);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return Workspace.__wrap(ret[0]);
    }
    /**
     * Boot a **devcontainer** workspace: the Boot Orchestrator
     * ([`MachineSpec`](holospaces::machine::MachineSpec)) generates the device
     * tree and boots `kernel` on a machine whose `virtio-blk` disk is the
     * assembled `rootfs` (from [`DevcontainerImage::assemble`]). The guest
     * kernel mounts the rootfs over `/dev/vda` and runs the devcontainer's real
     * OS — entirely in the browser peer (`CC-14`). Drive it with
     * [`run`](Workspace::run), exactly like [`boot`](Workspace::boot).
     * @param {Uint8Array} kernel
     * @param {Uint8Array} rootfs
     * @returns {Workspace}
     */
    static boot_devcontainer(kernel, rootfs) {
        const ptr0 = passArray8ToWasm0(kernel, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passArray8ToWasm0(rootfs, wasm.__wbindgen_malloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.workspace_boot_devcontainer(ptr0, len0, ptr1, len1);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return Workspace.__wrap(ret[0]);
    }
    /**
     * Boot a devcontainer with the **in-process loopback bridge** enabled
     * (ADR-020, `CC-33`): the guest's interface comes up with DHCP (so it has a
     * real TCP stack), but instead of a WebSocket egress to the internet it gets
     * a no-op egress and the *loopback ingress* — so the workbench, in this same
     * process, can [`dial_guest`](Workspace::dial_guest) a server *inside* the
     * devcontainer (a language server, a remote extension host) and exchange a
     * byte stream with it, with no relay or socket. This is the transport the VS
     * Code remote model runs over in the browser peer (ADR-015/ADR-020). Drive it
     * with [`run`](Workspace::run), pumping the NAT so the bridge's bytes flow.
     * @param {Uint8Array} kernel
     * @param {Uint8Array} rootfs
     * @returns {Workspace}
     */
    static boot_devcontainer_bridged(kernel, rootfs) {
        const ptr0 = passArray8ToWasm0(kernel, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passArray8ToWasm0(rootfs, wasm.__wbindgen_malloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.workspace_boot_devcontainer_bridged(ptr0, len0, ptr1, len1);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return Workspace.__wrap(ret[0]);
    }
    /**
     * Boot a **networked** devcontainer workspace (`CC-16`): like
     * [`boot_devcontainer`](Workspace::boot_devcontainer), but the machine also
     * has a `virtio-net` device whose userspace TCP/IP NAT tunnels the guest's
     * TCP streams out over a WebSocket to the relay at `relay_url` (there is no
     * raw NIC behind a tab; ADR-014). The guest brings its interface up with
     * DHCP and can then reach the internet — `git clone`, `apt`, `npm` — from the
     * browser peer. Drive it with [`run`](Workspace::run), yielding to the event
     * loop between chunks so the WebSocket delivers host-side bytes.
     * @param {Uint8Array} kernel
     * @param {Uint8Array} rootfs
     * @param {string} relay_url
     * @returns {Workspace}
     */
    static boot_devcontainer_net(kernel, rootfs, relay_url) {
        const ptr0 = passArray8ToWasm0(kernel, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passArray8ToWasm0(rootfs, wasm.__wbindgen_malloc);
        const len1 = WASM_VECTOR_LEN;
        const ptr2 = passStringToWasm0(relay_url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len2 = WASM_VECTOR_LEN;
        const ret = wasm.workspace_boot_devcontainer_net(ptr0, len0, ptr1, len1, ptr2, len2);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return Workspace.__wrap(ret[0]);
    }
    /**
     * The κ of every operator event published on the terminal channel so far.
     * @returns {any[]}
     */
    channel() {
        const ret = wasm.workspace_channel(this.__wbg_ptr);
        var v1 = getArrayJsValueFromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
        return v1;
    }
    /**
     * Dial an in-process connection to a server *inside* the devcontainer,
     * listening on `guest_port`, over the loopback substrate bridge (ADR-020,
     * `CC-33`). Returns the connection id, or `None` if the machine was not booted
     * with the bridge ([`boot_devcontainer_bridged`](Workspace::boot_devcontainer_bridged)).
     * The workbench uses this to reach a language server / the remote extension
     * host (ADR-015) without a relay or socket. Pump with [`run`](Workspace::run)
     * so the NAT opens the connection and the byte stream flows.
     * @param {number} guest_port
     * @returns {number | undefined}
     */
    dial_guest(guest_port) {
        const ret = wasm.workspace_dial_guest(this.__wbg_ptr, guest_port);
        return ret === Number.MAX_SAFE_INTEGER ? undefined : ret;
    }
    /**
     * Feed **raw terminal input** to the running holospace — the bytes an
     * interactive terminal delivers for each keystroke, *unbuffered*: ordinary
     * characters, control bytes (Ctrl-C = `0x03`, Ctrl-D = `0x04`), and escape
     * sequences (arrows, Home/End). Unlike [`Workspace::type_line`] this does not
     * line-buffer or block: the bytes go to the guest console and the caller's
     * render loop ([`Workspace::run`] + [`Workspace::terminal_delta`]) advances
     * the machine, so the guest's own tty echoes and edits the line and Ctrl-C
     * raises SIGINT — a real terminal, not a line submitter. The input is part of
     * the machine's canonical state (it is captured in the κ snapshot), so the
     * session stays reproducible (Law L1).
     * @param {Uint8Array} bytes
     */
    feed_input(bytes) {
        const ptr0 = passArray8ToWasm0(bytes, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.workspace_feed_input(this.__wbg_ptr, ptr0, len0);
    }
    /**
     * The **file tree**: the workspace's files as a JSON array of
     * `{ path, kappa }` — each file's current content κ (its identity, Law L1).
     * What the editor's explorer renders.
     * @returns {string}
     */
    files() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.workspace_files(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * Close the host side of a loopback connection (`CC-33`).
     * @param {number} id
     */
    guest_close(id) {
        wasm.workspace_guest_close(this.__wbg_ptr, id);
    }
    /**
     * Whether a loopback connection is still usable — the guest has not closed it,
     * or has but unread bytes remain (`CC-33`).
     * @param {number} id
     * @returns {boolean}
     */
    guest_is_open(id) {
        const ret = wasm.workspace_guest_is_open(this.__wbg_ptr, id);
        return ret !== 0;
    }
    /**
     * Drain the guest server's reply bytes on a loopback connection (empty until
     * the machine is pumped enough for the stream to advance; `CC-33`).
     * @param {number} id
     * @returns {Uint8Array}
     */
    guest_recv(id) {
        const ret = wasm.workspace_guest_recv(this.__wbg_ptr, id);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * Write bytes toward the guest server on a loopback connection (`CC-33`).
     * @param {number} id
     * @param {Uint8Array} data
     */
    guest_send(id, data) {
        const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.workspace_guest_send(this.__wbg_ptr, id, ptr0, len0);
    }
    /**
     * Whether the machine has powered off.
     * @returns {boolean}
     */
    get halted() {
        const ret = wasm.workspace_halted(this.__wbg_ptr);
        return ret !== 0;
    }
    /**
     * The editor's read: fetch a file's content *by κ*, verifying it by
     * re-derivation (Law L5). `undefined` if it is not in the workspace store.
     * @param {string} kappa
     * @returns {Uint8Array | undefined}
     */
    open_file(kappa) {
        const ptr0 = passStringToWasm0(kappa, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.workspace_open_file(this.__wbg_ptr, ptr0, len0);
        if (ret[3]) {
            throw takeFromExternrefTable0(ret[2]);
        }
        let v2;
        if (ret[0] !== 0) {
            v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
            wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        }
        return v2;
    }
    /**
     * Open a file *by path*: the content at the file's current κ (the editor
     * reads the environment content by κ). `undefined` if the path is unknown.
     * @param {string} path
     * @returns {Uint8Array | undefined}
     */
    read_path(path) {
        const ptr0 = passStringToWasm0(path, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.workspace_read_path(this.__wbg_ptr, ptr0, len0);
        if (ret[3]) {
            throw takeFromExternrefTable0(ret[2]);
        }
        let v2;
        if (ret[0] !== 0) {
            v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
            wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        }
        return v2;
    }
    /**
     * **Apply a configuration** the control plane published (ADR-018; `CC-28`):
     * decode the κ-addressed [`Configuration`] bytes (resolved + verified over
     * the substrate by the caller, Law L5) and enact its live directives on the
     * *running* machine — each `forwardPort` begins forwarding on the running
     * instance, without a reboot. Returns a JSON summary of what was applied
     * (`{ "forwarded": [{ "guest": 8080, "host": 8080 }], "lifecycle": "…",
     * "unsupported": [...] }`). The instance state changes from the panel's
     * configuration, carried as content over the substrate — no RPC.
     * @param {Uint8Array} config_bytes
     * @returns {string}
     */
    reconfigure(config_bytes) {
        let deferred3_0;
        let deferred3_1;
        try {
            const ptr0 = passArray8ToWasm0(config_bytes, wasm.__wbindgen_malloc);
            const len0 = WASM_VECTOR_LEN;
            const ret = wasm.workspace_reconfigure(this.__wbg_ptr, ptr0, len0);
            var ptr2 = ret[0];
            var len2 = ret[1];
            if (ret[3]) {
                ptr2 = 0; len2 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred3_0 = ptr2;
            deferred3_1 = len2;
            return getStringFromWasm0(ptr2, len2);
        } finally {
            wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
        }
    }
    /**
     * Resume a devcontainer workspace from a κ snapshot [`suspend`](Workspace::suspend)
     * produced, instead of cold-booting it (`CC-30`). The running OS, its disk,
     * and the workspace files come back exactly — so a second launch skips the
     * boot entirely and the editor's content is intact. The snapshot's integrity
     * is the caller's to check by re-derivation before trusting it across a
     * session boundary (Law L5; ADR-019) — OPFS is durable but untrusted storage.
     * @param {Uint8Array} snapshot
     * @returns {Workspace}
     */
    static resume_devcontainer(snapshot) {
        const ptr0 = passArray8ToWasm0(snapshot, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.workspace_resume_devcontainer(ptr0, len0);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return Workspace.__wrap(ret[0]);
    }
    /**
     * Advance the running holospace by `budget` instructions (one chunk of the
     * boot or of servicing input). Returns `true` once the machine has halted
     * (powered off). Call repeatedly from a UI loop, rendering
     * [`terminal`](Workspace::terminal) between chunks.
     * @param {number} budget
     * @returns {boolean}
     */
    run(budget) {
        const ret = wasm.workspace_run(this.__wbg_ptr, budget);
        return ret !== 0;
    }
    /**
     * The **editor** surface: save a file's content (the operator's edit). The
     * content is κ-addressed into the substrate (Law L2), so the returned κ is
     * the file's new identity — an edit advances it (Law L1). The canonical edit
     * event for `path` is published on the channel.
     * @param {string} path
     * @param {Uint8Array} content
     * @returns {string}
     */
    save_file(path, content) {
        let deferred4_0;
        let deferred4_1;
        try {
            const ptr0 = passStringToWasm0(path, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passArray8ToWasm0(content, wasm.__wbindgen_malloc);
            const len1 = WASM_VECTOR_LEN;
            const ret = wasm.workspace_save_file(this.__wbg_ptr, ptr0, len0, ptr1, len1);
            var ptr3 = ret[0];
            var len3 = ret[1];
            if (ret[3]) {
                ptr3 = 0; len3 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred4_0 = ptr3;
            deferred4_1 = len3;
            return getStringFromWasm0(ptr3, len3);
        } finally {
            wasm.__wbindgen_free(deferred4_0, deferred4_1, 1);
        }
    }
    /**
     * Whether the terminal has rendered `marker` yet (e.g. the ready banner).
     * @param {string} marker
     * @returns {boolean}
     */
    shows(marker) {
        const ptr0 = passStringToWasm0(marker, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.workspace_shows(this.__wbg_ptr, ptr0, len0);
        return ret !== 0;
    }
    /**
     * The running holospace's κ snapshot — its canonical state (Law L1/L3/L5).
     * @returns {string}
     */
    state_kappa() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.workspace_state_kappa(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * Suspend the running machine to a κ snapshot — the canonical,
     * content-addressed bytes of the whole machine: CPU, RAM, the rootfs disk,
     * and the *workspace files* (virtio-9p). The browser persists these (gzipped)
     * to OPFS so the next launch *resumes* instead of cold-booting (`CC-30`).
     * Most of guest RAM is zero, so the gzipped snapshot is a small fraction of
     * the machine size.
     * @returns {Uint8Array}
     */
    suspend() {
        const ret = wasm.workspace_suspend(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * The rendered terminal — the console the running holospace has produced.
     * @returns {string}
     */
    terminal() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.workspace_terminal(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * The console bytes produced **since the last call** (an internal cursor),
     * for the integrated terminal's render loop. Returning only the delta avoids
     * re-reading and re-encoding the whole console each tick — output stays O(new
     * bytes), not O(total) per frame. Returns raw bytes (the terminal decodes
     * them); [`Workspace::terminal`] still returns the full buffer for tests.
     * @returns {Uint8Array}
     */
    terminal_delta() {
        const ret = wasm.workspace_terminal_delta(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * Type a line into the terminal: publish it as a canonical event on the
     * holospace's channel (Law L1/L2), feed the keystrokes to the running
     * machine, and run until the response settles. The holospace's κ snapshot
     * advances. Returns the event's κ.
     * @param {string} line
     * @returns {string}
     */
    type_line(line) {
        let deferred2_0;
        let deferred2_1;
        try {
            const ptr0 = passStringToWasm0(line, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len0 = WASM_VECTOR_LEN;
            const ret = wasm.workspace_type_line(this.__wbg_ptr, ptr0, len0);
            deferred2_0 = ret[0];
            deferred2_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred2_0, deferred2_1, 1);
        }
    }
    /**
     * Delete a file or folder from the shared workspace (the workbench
     * `FileSystemProvider.delete`) — the editor removing content the OS sees
     * over `virtio-9p`. `true` if it existed.
     * @param {string} name
     * @returns {boolean}
     */
    ws_delete(name) {
        const ptr0 = passStringToWasm0(name, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.workspace_ws_delete(this.__wbg_ptr, ptr0, len0);
        return ret !== 0;
    }
    /**
     * The shared workspace's directory listing — a JSON array of
     * `{ name, dir, size }` over the running holospace's `virtio-9p` workspace
     * (the workbench `FileSystemProvider.readDirectory`).
     * @returns {string}
     */
    ws_list() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.workspace_ws_list(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * Create a folder in the shared workspace (the workbench
     * `FileSystemProvider.createDirectory`).
     * @param {string} name
     */
    ws_mkdir(name) {
        const ptr0 = passStringToWasm0(name, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.workspace_ws_mkdir(this.__wbg_ptr, ptr0, len0);
    }
    /**
     * Read a file from the shared workspace (the workbench
     * `FileSystemProvider.readFile`) — the same content the OS reads over
     * `virtio-9p`. `undefined` if absent.
     * @param {string} name
     * @returns {Uint8Array | undefined}
     */
    ws_read(name) {
        const ptr0 = passStringToWasm0(name, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.workspace_ws_read(this.__wbg_ptr, ptr0, len0);
        let v2;
        if (ret[0] !== 0) {
            v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
            wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        }
        return v2;
    }
    /**
     * Rename a file or folder in the shared workspace (the workbench
     * `FileSystemProvider.rename`). `true` if the source existed.
     * @param {string} from
     * @param {string} to
     * @returns {boolean}
     */
    ws_rename(from, to) {
        const ptr0 = passStringToWasm0(from, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(to, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.workspace_ws_rename(this.__wbg_ptr, ptr0, len0, ptr1, len1);
        return ret !== 0;
    }
    /**
     * Write a file into the shared workspace (the workbench
     * `FileSystemProvider.writeFile`) — the editor saving the *same content* the
     * OS reads over `virtio-9p` (one content, Law L1). Returns the content's κ
     * (its identity, Law L1/L2).
     * @param {string} name
     * @param {Uint8Array} content
     * @returns {string}
     */
    ws_write(name, content) {
        let deferred3_0;
        let deferred3_1;
        try {
            const ptr0 = passStringToWasm0(name, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passArray8ToWasm0(content, wasm.__wbindgen_malloc);
            const len1 = WASM_VECTOR_LEN;
            const ret = wasm.workspace_ws_write(this.__wbg_ptr, ptr0, len0, ptr1, len1);
            deferred3_0 = ret[0];
            deferred3_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
        }
    }
}
if (Symbol.dispose) Workspace.prototype[Symbol.dispose] = Workspace.prototype.free;

/**
 * The κ-label of bytes on the substrate's default σ-axis (blake3) — the same
 * content address every peer computes (Law L1).
 * @param {Uint8Array} bytes
 * @returns {string}
 */
export function kappa(bytes) {
    let deferred2_0;
    let deferred2_1;
    try {
        const ptr0 = passArray8ToWasm0(bytes, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.kappa(ptr0, len0);
        deferred2_0 = ret[0];
        deferred2_1 = ret[1];
        return getStringFromWasm0(ret[0], ret[1]);
    } finally {
        wasm.__wbindgen_free(deferred2_0, deferred2_1, 1);
    }
}

/**
 * Run a `.holo` compute artifact in the browser via the hologram executor
 * compiled to wasm — the *browser `.holo` engine* (arc42 chapter 11, RT2;
 * conformance `CC-2`). Returns the κ-label of the first output. Because the
 * executor is deterministic and content-addressed, this κ equals the one the
 * native executor produces for the same `.holo` (the browser engine equals the
 * native one).
 * @param {Uint8Array} archive
 * @returns {string}
 */
export function run_holo(archive) {
    let deferred3_0;
    let deferred3_1;
    try {
        const ptr0 = passArray8ToWasm0(archive, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.run_holo(ptr0, len0);
        var ptr2 = ret[0];
        var len2 = ret[1];
        if (ret[3]) {
            ptr2 = 0; len2 = 0;
            throw takeFromExternrefTable0(ret[2]);
        }
        deferred3_0 = ptr2;
        deferred3_1 = len2;
        return getStringFromWasm0(ptr2, len2);
    } finally {
        wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
    }
}

/**
 * Validate that `module` is a recompiled userland fit for the *execution
 * surface* (ADR-008; `CC-6`): specification-valid WebAssembly that imports only
 * the substrate host ABI and presents the container ABI. This is the κ-boundary
 * contract the browser peer enforces before a userland may be a holospace's
 * code — ambient (WASI-style) imports and a missing container ABI are refused.
 * @param {Uint8Array} module
 */
export function validate_userland(module) {
    const ptr0 = passArray8ToWasm0(module, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.validate_userland(ptr0, len0);
    if (ret[1]) {
        throw takeFromExternrefTable0(ret[0]);
    }
}

/**
 * Verify bytes against a claimed κ-label by re-derivation (Law L5). This is
 * what makes content fetched from an untrusted gateway safe.
 * @param {Uint8Array} bytes
 * @param {string} kappa
 * @returns {boolean}
 */
export function verify_kappa(bytes, kappa) {
    const ptr0 = passArray8ToWasm0(bytes, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ptr1 = passStringToWasm0(kappa, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len1 = WASM_VECTOR_LEN;
    const ret = wasm.verify_kappa(ptr0, len0, ptr1, len1);
    if (ret[2]) {
        throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] !== 0;
}
function __wbg_get_imports() {
    const import0 = {
        __proto__: null,
        __wbg___wbindgen_throw_1506f2235d1bdba0: function(arg0, arg1) {
            throw new Error(getStringFromWasm0(arg0, arg1));
        },
        __wbg__wbg_cb_unref_61db23ac97f16c31: function(arg0) {
            arg0._wbg_cb_unref();
        },
        __wbg_data_bd354b70c783c66e: function(arg0) {
            const ret = arg0.data;
            return ret;
        },
        __wbg_length_4a591ecaa01354d9: function(arg0) {
            const ret = arg0.length;
            return ret;
        },
        __wbg_new_578aeef4b6b94378: function(arg0) {
            const ret = new Uint8Array(arg0);
            return ret;
        },
        __wbg_new_d7e476b433a26bea: function() { return handleError(function (arg0, arg1) {
            const ret = new WebSocket(getStringFromWasm0(arg0, arg1));
            return ret;
        }, arguments); },
        __wbg_prototypesetcall_3249fc62a0fafa30: function(arg0, arg1, arg2) {
            Uint8Array.prototype.set.call(getArrayU8FromWasm0(arg0, arg1), arg2);
        },
        __wbg_send_4a773f523104d75e: function() { return handleError(function (arg0, arg1, arg2) {
            arg0.send(getArrayU8FromWasm0(arg1, arg2));
        }, arguments); },
        __wbg_set_binaryType_41994c453b95bdd2: function(arg0, arg1) {
            arg0.binaryType = __wbindgen_enum_BinaryType[arg1];
        },
        __wbg_set_onclose_13787fb31ae8aefd: function(arg0, arg1) {
            arg0.onclose = arg1;
        },
        __wbg_set_onmessage_9c6b4cb14e244b7f: function(arg0, arg1) {
            arg0.onmessage = arg1;
        },
        __wbg_set_onopen_db452f4233e99d7d: function(arg0, arg1) {
            arg0.onopen = arg1;
        },
        __wbindgen_cast_0000000000000001: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [Externref], shim_idx: 8, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, wasm_bindgen__convert__closures_____invoke__h433d5e4bea598df1);
            return ret;
        },
        __wbindgen_cast_0000000000000002: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [NamedExternref("MessageEvent")], shim_idx: 8, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, wasm_bindgen__convert__closures_____invoke__h433d5e4bea598df1_1);
            return ret;
        },
        __wbindgen_cast_0000000000000003: function(arg0, arg1) {
            // Cast intrinsic for `Ref(String) -> Externref`.
            const ret = getStringFromWasm0(arg0, arg1);
            return ret;
        },
        __wbindgen_init_externref_table: function() {
            const table = wasm.__wbindgen_externrefs;
            const offset = table.grow(4);
            table.set(0, undefined);
            table.set(offset + 0, undefined);
            table.set(offset + 1, null);
            table.set(offset + 2, true);
            table.set(offset + 3, false);
        },
    };
    return {
        __proto__: null,
        "./holospaces_web_bg.js": import0,
    };
}

function wasm_bindgen__convert__closures_____invoke__h433d5e4bea598df1(arg0, arg1, arg2) {
    wasm.wasm_bindgen__convert__closures_____invoke__h433d5e4bea598df1(arg0, arg1, arg2);
}

function wasm_bindgen__convert__closures_____invoke__h433d5e4bea598df1_1(arg0, arg1, arg2) {
    wasm.wasm_bindgen__convert__closures_____invoke__h433d5e4bea598df1_1(arg0, arg1, arg2);
}


const __wbindgen_enum_BinaryType = ["blob", "arraybuffer"];
const ConsoleFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_console_free(ptr, 1));
const DevcontainerImageFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_devcontainerimage_free(ptr, 1));
const WorkspaceFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_workspace_free(ptr, 1));

function addToExternrefTable0(obj) {
    const idx = wasm.__externref_table_alloc();
    wasm.__wbindgen_externrefs.set(idx, obj);
    return idx;
}

const CLOSURE_DTORS = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(state => wasm.__wbindgen_destroy_closure(state.a, state.b));

function getArrayJsValueFromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    const mem = getDataViewMemory0();
    const result = [];
    for (let i = ptr; i < ptr + 4 * len; i += 4) {
        result.push(wasm.__wbindgen_externrefs.get(mem.getUint32(i, true)));
    }
    wasm.__externref_drop_slice(ptr, len);
    return result;
}

function getArrayU8FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
}

let cachedDataViewMemory0 = null;
function getDataViewMemory0() {
    if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer.detached === true || (cachedDataViewMemory0.buffer.detached === undefined && cachedDataViewMemory0.buffer !== wasm.memory.buffer)) {
        cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
    }
    return cachedDataViewMemory0;
}

function getStringFromWasm0(ptr, len) {
    return decodeText(ptr >>> 0, len);
}

let cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
    if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
        cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
    }
    return cachedUint8ArrayMemory0;
}

function handleError(f, args) {
    try {
        return f.apply(this, args);
    } catch (e) {
        const idx = addToExternrefTable0(e);
        wasm.__wbindgen_exn_store(idx);
    }
}

function makeMutClosure(arg0, arg1, f) {
    const state = { a: arg0, b: arg1, cnt: 1 };
    const real = (...args) => {

        // First up with a closure we increment the internal reference
        // count. This ensures that the Rust closure environment won't
        // be deallocated while we're invoking it.
        state.cnt++;
        const a = state.a;
        state.a = 0;
        try {
            return f(a, state.b, ...args);
        } finally {
            state.a = a;
            real._wbg_cb_unref();
        }
    };
    real._wbg_cb_unref = () => {
        if (--state.cnt === 0) {
            wasm.__wbindgen_destroy_closure(state.a, state.b);
            state.a = 0;
            CLOSURE_DTORS.unregister(state);
        }
    };
    CLOSURE_DTORS.register(real, state, state);
    return real;
}

function passArray8ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 1, 1) >>> 0;
    getUint8ArrayMemory0().set(arg, ptr / 1);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passStringToWasm0(arg, malloc, realloc) {
    if (realloc === undefined) {
        const buf = cachedTextEncoder.encode(arg);
        const ptr = malloc(buf.length, 1) >>> 0;
        getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
        WASM_VECTOR_LEN = buf.length;
        return ptr;
    }

    let len = arg.length;
    let ptr = malloc(len, 1) >>> 0;

    const mem = getUint8ArrayMemory0();

    let offset = 0;

    for (; offset < len; offset++) {
        const code = arg.charCodeAt(offset);
        if (code > 0x7F) break;
        mem[ptr + offset] = code;
    }
    if (offset !== len) {
        if (offset !== 0) {
            arg = arg.slice(offset);
        }
        ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
        const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
        const ret = cachedTextEncoder.encodeInto(arg, view);

        offset += ret.written;
        ptr = realloc(ptr, len, offset, 1) >>> 0;
    }

    WASM_VECTOR_LEN = offset;
    return ptr;
}

function takeFromExternrefTable0(idx) {
    const value = wasm.__wbindgen_externrefs.get(idx);
    wasm.__externref_table_dealloc(idx);
    return value;
}

let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
cachedTextDecoder.decode();
const MAX_SAFARI_DECODE_BYTES = 2146435072;
let numBytesDecoded = 0;
function decodeText(ptr, len) {
    numBytesDecoded += len;
    if (numBytesDecoded >= MAX_SAFARI_DECODE_BYTES) {
        cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
        cachedTextDecoder.decode();
        numBytesDecoded = len;
    }
    return cachedTextDecoder.decode(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}

const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        const buf = cachedTextEncoder.encode(arg);
        view.set(buf);
        return {
            read: arg.length,
            written: buf.length
        };
    };
}

let WASM_VECTOR_LEN = 0;

let wasmModule, wasmInstance, wasm;
function __wbg_finalize_init(instance, module) {
    wasmInstance = instance;
    wasm = instance.exports;
    wasmModule = module;
    cachedDataViewMemory0 = null;
    cachedUint8ArrayMemory0 = null;
    wasm.__wbindgen_start();
    return wasm;
}

async function __wbg_load(module, imports) {
    if (typeof Response === 'function' && module instanceof Response) {
        if (typeof WebAssembly.instantiateStreaming === 'function') {
            try {
                return await WebAssembly.instantiateStreaming(module, imports);
            } catch (e) {
                const validResponse = module.ok && expectedResponseType(module.type);

                if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                    console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                } else { throw e; }
            }
        }

        const bytes = await module.arrayBuffer();
        return await WebAssembly.instantiate(bytes, imports);
    } else {
        const instance = await WebAssembly.instantiate(module, imports);

        if (instance instanceof WebAssembly.Instance) {
            return { instance, module };
        } else {
            return instance;
        }
    }

    function expectedResponseType(type) {
        switch (type) {
            case 'basic': case 'cors': case 'default': return true;
        }
        return false;
    }
}

function initSync(module) {
    if (wasm !== undefined) return wasm;


    if (module !== undefined) {
        if (Object.getPrototypeOf(module) === Object.prototype) {
            ({module} = module)
        } else {
            console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
        }
    }

    const imports = __wbg_get_imports();
    if (!(module instanceof WebAssembly.Module)) {
        module = new WebAssembly.Module(module);
    }
    const instance = new WebAssembly.Instance(module, imports);
    return __wbg_finalize_init(instance, module);
}

async function __wbg_init(module_or_path) {
    if (wasm !== undefined) return wasm;


    if (module_or_path !== undefined) {
        if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
            ({module_or_path} = module_or_path)
        } else {
            console.warn('using deprecated parameters for the initialization function; pass a single object instead')
        }
    }

    if (module_or_path === undefined) {
        module_or_path = new URL('holospaces_web_bg.wasm', import.meta.url);
    }
    const imports = __wbg_get_imports();

    if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
        module_or_path = fetch(module_or_path);
    }

    const { instance, module } = await __wbg_load(await module_or_path, imports);

    return __wbg_finalize_init(instance, module);
}

export { initSync, __wbg_init as default };
