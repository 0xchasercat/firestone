---
icon: fire
description: Run Linux VMs and container images on Cloud Hypervisor, from one binary.
layout:
  title:
    visible: true
  description:
    visible: true
  tableOfContents:
    visible: true
  outline:
    visible: true
  pagination:
    visible: true
---

# Welcome to Firestone

Firestone runs Linux virtual machines on Cloud Hypervisor, and it runs OCI container images as virtual machines. One executable carries the VMM, `passt` and `qemu-img` inside it. It runs as your user, nothing keeps running between your commands, and every machine is a directory of files you can read.

<button type="button" class="button primary" data-action="ask" data-icon="gitbook-assistant">Ask the Firestone docs</button> <button type="button" class="button secondary" data-action="ask" data-query="How do I boot my first machine?" data-icon="bolt">First machine</button> <button type="button" class="button secondary" data-action="ask" data-query="How do I run a Docker image as a VM?" data-icon="box">Run a container image</button>

## Where to start

<table data-view="cards"><thead><tr><th></th><th></th><th></th><th data-hidden data-card-target data-type="content-ref"></th></tr></thead><tbody><tr><td><h3><i class="fa-download" style="color:$primary;">:download:</i></h3></td><td><h3><strong>Install</strong></h3></td><td>One command installs the release binary. No root, no daemon, no packages.</td><td><a href="install.md">Install</a></td></tr><tr><td><h3><i class="fa-bolt" style="color:$primary;">:bolt:</i></h3></td><td><h3><strong>Quick start</strong></h3></td><td>Check the host, boot Ubuntu, get a shell. About two minutes on a warm cache.</td><td><a href="quickstart.md">Quick start</a></td></tr><tr><td><h3><i class="fa-server" style="color:$primary;">:server:</i></h3></td><td><h3><strong>Machines</strong></h3></td><td>Lifecycle, snapshots, clone, live resize, logs and metrics.</td><td><a href="machines.md">Machines</a></td></tr><tr><td><h3><i class="fa-layer-group" style="color:$primary;">:layer-group:</i></h3></td><td><h3><strong>Images</strong></h3></td><td>Cloud images from the catalog, plus any OCI image from a registry.</td><td><a href="images.md">Images</a></td></tr></tbody></table>

## Go deeper

<table data-view="cards"><thead><tr><th></th><th></th><th></th><th data-hidden data-card-target data-type="content-ref"></th></tr></thead><tbody><tr><td><h3><i class="fa-browser" style="color:$primary;">:browser:</i></h3></td><td><h3><strong>Web interface</strong></h3></td><td>The embedded dashboard with a real terminal, served by <code>firestone ui</code>.</td><td><a href="web-ui.md">Web interface</a></td></tr><tr><td><h3><i class="fa-network-wired" style="color:$primary;">:network-wired:</i></h3></td><td><h3><strong>Networking</strong></h3></td><td>Port forwards, tap mode and shared folders.</td><td><a href="networking.md">Networking</a></td></tr><tr><td><h3><i class="fa-cloud" style="color:$primary;">:cloud:</i></h3></td><td><h3><strong>Cloud-init</strong></h3></td><td>SSH keys, passwords, user data and static addressing.</td><td><a href="cloud-init.md">Cloud-init</a></td></tr><tr><td><h3><i class="fa-terminal" style="color:$primary;">:terminal:</i></h3></td><td><h3><strong>CLI and REST</strong></h3></td><td>Every command, the REST server and the <code>--json</code> conventions.</td><td><a href="cli-and-rest.md">CLI and REST</a></td></tr><tr><td><h3><i class="fa-code" style="color:$primary;">:code:</i></h3></td><td><h3><strong>API reference</strong></h3></td><td>Every REST operation, generated from the tested OpenAPI contract.</td><td><a href="api-reference/README.md">API reference</a></td></tr><tr><td><h3><i class="fa-wrench" style="color:$primary;">:wrench:</i></h3></td><td><h3><strong>Troubleshooting</strong></h3></td><td>The symptom table, plus the <a href="security.md">security model</a> behind the defaults.</td><td><a href="troubleshooting.md">Troubleshooting</a></td></tr></tbody></table>

## Platform

Firestone targets Linux x86\_64 hosts with KVM. The aarch64 target compiles and its catalog metadata exists, but there is no aarch64 runtime release and `doctor` refuses an aarch64 host. Non-Linux hosts, non-Linux guests, and cross-architecture emulation are not planned.

[`openapi.json`](openapi.json) is the OpenAPI 3.1 contract for the REST API.
