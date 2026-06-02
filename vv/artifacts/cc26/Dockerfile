# A devcontainer Dockerfile (the CC-26 witness build context). Busybox-runnable:
# FROM the busybox base, an ENV the RUN expands, a WORKDIR, a COPY from the build
# context, and RUN steps (one runs the copied script, one echoes a marker).
ARG TAG=latest
FROM holospaces/busybox:${TAG}
ENV BUILT_BY=cc26 LANG=C.UTF-8
WORKDIR /workspace
COPY setup.sh /usr/local/bin/setup.sh
RUN busybox sh /usr/local/bin/setup.sh
RUN echo BUILD-RAN:$BUILT_BY in $(pwd)
