# Proposal: managed host listeners for UDS-backed vsock

## Summary

Add an optional, VMM-managed host listener protocol to `UdsVsock`.  A host
program registers a vsock port through the main UDS, rather than binding a
separate `"<uds_path>_<port>"` socket.  When a guest connects to that port,
the VMM creates a Unix stream pair, passes one file descriptor to the host
listener with `SCM_RIGHTS`, and forwards the other endpoint to the guest over
virtio-vsock.

This gives the VMM ownership of the host-port namespace and lets it enforce
port-conflict semantics.

## Background

The current UDS backend supports two connection directions:

* For host-initiated connections, the host connects to `<uds_path>` and sends
  `CONNECT <guest_port>`.  The VMM assigns a host-side port.
* For guest-initiated connections, the VMM connects to a host-created listener
  at `<uds_path>_<host_port>`.

The latter listener is created independently of the VMM.  Consequently, the
VMM cannot make a host `bind("<uds_path>_<host_port>")` fail when that port is
already used by another vsock connection.  The current `host_ports` map also
counts connections rather than reserving a unique host-port namespace, so it
does not reject this overlap.

This is a limitation of path-per-port UDS multiplexing.  It is acceptable for
applications that treat the two directions as independent namespaces, but it
does not faithfully model a single host-side AF_VSOCK port namespace.

## Goals

* Give the VMM authority to reserve and release host vsock ports.
* Return a deterministic conflict error when a host port is already reserved.
* Let host programs accept guest-initiated connections without creating a
  suffix UDS path for every port.
* Preserve the existing `CONNECT <guest_port>` protocol and path-per-port
  listener behavior for compatibility.
* Keep the VMM worker non-blocking when the host application accepts slowly.

## Non-goals

* Replacing the existing path-per-port listener protocol immediately.
* Changing the guest virtio-vsock wire protocol.
* Providing a general-purpose host AF_VSOCK implementation.

## Proposed protocol

The main UDS gains a long-lived control-connection mode.

### Register a listener

The host connects to `<uds_path>` and sends:

```text
LISTEN <host_port>\n
```

The VMM validates and reserves `host_port`, then replies with one of:

```text
OK\n
ERR EADDRINUSE\n
ERR <reason>\n
```

The UDS remains a control connection after `OK`.  Closing it unregisters the
listener and releases the port.

### Accept a connection

When the guest sends a `VIRTIO_VSOCK_OP_REQUEST` to a registered host port,
the VMM:

1. creates `UnixStream::pair()`;
2. retains one endpoint and associates it with the guest vsock connection;
3. queues the other endpoint for the registered host listener; and
4. sends a framed accept notification and that endpoint with `SCM_RIGHTS`.

The notification includes enough peer information for a host-side API to
return an accepted connection, for example:

```text
ACCEPT <guest_cid> <guest_port>\n
```

The host-side `Accept()` waits in `recvmsg()` for this notification and returns
the received fd as the connected stream.  The VMM sends the virtio-vsock
`RESPONSE` only after it has successfully queued (or transferred; see open
questions) the accepted endpoint.

### Existing host-initiated connections

`CONNECT <guest_port>` remains unchanged.  It uses the connected main UDS as
its data stream after the `OK <assigned_host_port>` response, as it does today.

## Port ownership and conflict rules

The VMM maintains one host-port table with an explicit owner:

* `Connected`: assigned as the host source port of a host-initiated
  connection.
* `Listener`: reserved by a successful `LISTEN` request.

Initially, a `LISTEN` request conflicts with either owner, and an automatically
assigned host source port must skip listener-owned ports.  This models one
shared host-port namespace and prevents the collision that path-per-port UDS
listeners cannot prevent.

The legacy `<uds_path>_<port>` path is outside VMM control.  To avoid ambiguous
behavior, managed listeners take precedence: a guest connection to a managed
port never attempts the legacy suffix path.  A later migration may disable the
legacy mode entirely.

## Backpressure and lifecycle

The VMM must not block on a listener's control socket.  Each listener has a
bounded pending-accept queue.  If it is full, or if passing the fd fails, the
VMM sends `RST` for the guest request and closes both ends of the socket pair.

When the listener control connection closes, the VMM:

1. unregisters the listener and releases its port;
2. rejects queued, not-yet-delivered guest requests with `RST`; and
3. keeps already accepted connections alive until their normal vsock teardown.

## Host-side API sketch

This protocol can be wrapped in a small client library:

```text
listener = Listener::bind("/tmp/vsock.sock", host_port)
stream, peer = listener.accept()
```

`bind` opens the control UDS, sends `LISTEN`, and waits for `OK`.  `accept`
uses `recvmsg` to receive both the framed `ACCEPT` record and its `SCM_RIGHTS`
file descriptor.

## Compatibility

No existing `CONNECT` client needs to change.  Existing guest-to-host clients
using `<uds_path>_<port>` can remain supported as a legacy fallback for ports
not registered through `LISTEN`.  New users opt in by using the managed
listener client API.

## Open questions

1. **Namespace semantics:** Must `LISTEN(port)` conflict with every active
   host-initiated connection using `port`, or should connected source ports and
   listener ports be separate namespaces for compatibility?
2. **Legacy collision policy:** If a legacy suffix listener already exists for
   a port, should `LISTEN(port)` succeed, fail after probing the path, or simply
   take precedence without probing?  Probing has an unavoidable TOCTOU race.
3. **Response timing:** Should the VMM send the guest `RESPONSE` after enqueue,
   after successful `sendmsg`, or only after the host application calls
   `Accept`?  These choices trade guest connection latency against failure
   reporting accuracy.
4. **Backlog:** What default queue length and configurability are appropriate?
   How should a full queue be exposed to the guest and host?
5. **Message framing:** `SOCK_STREAM` does not preserve message boundaries.
   Define a length-prefixed binary control protocol, or determine whether a
   portable `SOCK_SEQPACKET` transport is acceptable on supported platforms.
6. **FD-passing API:** Which low-level abstraction should own `sendmsg` and
   `recvmsg`, and how should it avoid buffered reads that can separate the
   control record from its ancillary data?
7. **Authentication:** Are UDS filesystem permissions sufficient to authorize
   listener registration, or does the protocol need peer-credential checks and
   a per-port authorization policy?
8. **Reconnect behavior:** If a host listener reconnects after a control-socket
   failure, can it reclaim the same port immediately?  What happens to queued
   and established connections?
9. **Observability:** Which counters and logs are needed for listener
   registration, conflicts, backlog overflow, fd-passing failures, and guest
   resets?
