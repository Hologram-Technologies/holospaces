FROM mcr.microsoft.com/devcontainers/base:ubuntu-24.04

ARG USERNAME=vscode
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bat \
        build-essential \
        ca-certificates \
        cmake \
        curl \
        eza \
        gh \
        git \
        git-lfs \
        graphviz \
        just \
        less \
        lldb \
        openjdk-21-jdk \
        pkg-config \
        qemu-system-misc \
        qemu-utils \
        ripgrep \
        tree \
        vim \
        wget \
        zsh \
    && rm -rf /var/lib/apt/lists/* \
    # Ubuntu's lldb package ships the debug adapter as the versioned
    # /usr/bin/lldb-dap-18 with no unversioned symlink. The lldb-dap VS Code
    # extension (and devcontainer.json's lldb-dap.executable-path) expects
    # /usr/bin/lldb-dap, so provide it.
    && ln -sf lldb-dap-18 /usr/bin/lldb-dap

# Rust toolchain. Installed here rather than via ghcr.io/devcontainers/features/rust
# because that feature force-bundles the CodeLLDB extension (vadimcn.vscode-lldb),
# which fails to load under Cursor (its platform-package installer calls a
# workbench command Cursor lacks). The devcontainer spec only unions feature
# extensions, so the feature can't be opted out of — hence the manual install.
# Mirrors the feature's layout: system-wide CARGO_HOME/RUSTUP_HOME, minimal
# profile, same components.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:${PATH}
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal \
            --default-toolchain stable \
            --component rust-analyzer,rust-src,rustfmt,clippy \
    && chmod -R a+w "${RUSTUP_HOME}" "${CARGO_HOME}"

RUN if [ ! -d "/home/${USERNAME}/.oh-my-zsh" ]; then \
        git clone --depth=1 https://github.com/ohmyzsh/ohmyzsh.git "/home/${USERNAME}/.oh-my-zsh"; \
    fi \
    && chsh -s /usr/bin/zsh "${USERNAME}" \
    && mkdir -p "/home/${USERNAME}/.config/git" \
    && chown -R "${USERNAME}:${USERNAME}" "/home/${USERNAME}/.oh-my-zsh" "/home/${USERNAME}/.config"

COPY --chown=${USERNAME}:${USERNAME} shell/zshrc /home/${USERNAME}/.zshrc
COPY --chown=${USERNAME}:${USERNAME} shell/aliases.zsh /home/${USERNAME}/.config/holospaces-aliases.zsh
COPY --chown=${USERNAME}:${USERNAME} shell/gitconfig /home/${USERNAME}/.config/git/holospaces.gitconfig
COPY --chown=${USERNAME}:${USERNAME} shell/gitconfig-home /home/${USERNAME}/.gitconfig
