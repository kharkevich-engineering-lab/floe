# floe Helm chart

floe on Kubernetes: one Deployment of disposable pods in front of one bucket. The chart is thin:
every template is a one-line call into the [`common`](https://github.com/technicaldomain/helm/tree/main/charts/common)
library chart (2.5.0), so `values.yaml` is the library's values plus `floeToml`, the bootstrap
`floe.toml`. Runtime configuration (GitHub mirror, catalog, events, codeintel, MCP) is not in the
chart: it lives in the bucket and is edited at `/_admin` (D60).

```sh
helm install git oci://ghcr.io/kharkevich-engineering-lab/charts/floe --version <release> -f my-values.yaml
```

The chart version, its `appVersion` and the image tag are the floe release. Resources are named
`<release>-floe` (`git-floe` above). `ci/` holds the values files CI renders; each is an example.

## Development with RustFS

`ci/kind/rustfs.yaml` runs RustFS and creates the `floe-test` bucket; `ci/dev-rustfs-values.yaml`
points floe at it with throwaway credentials from `secretData` (a Secret the chart creates; never
for production):

```sh
kubectl create namespace floe
kubectl -n floe apply -f ci/kind/rustfs.yaml
helm install floe . -n floe -f ci/dev-rustfs-values.yaml
kubectl -n floe port-forward svc/floe-floe 8097:80
git -c http.extraHeader="Authorization: Bearer floe-dev-admin-token" push http://127.0.0.1:8097/me/repo.git main
```

## Production on S3 with IRSA

No keys: annotate the ServiceAccount, leave `AWS_ACCESS_KEY_ID` unset and floe uses the SDK chain
(D43). Secrets come from a Secret you manage (`sharedSecrets`: every key becomes an env var).
Small differences from the default `floeToml` are easiest as `FLOE__SECTION__KEY` env vars:

```yaml
serviceAccount:
  annotations: {eks.amazonaws.com/role-arn: arn:aws:iam::123456789012:role/floe}
sharedSecrets: [floe-credentials]   # FLOE_TOKEN_ADMIN, FLOE_CONFIG_KEY, FLOE__SERVER__AUTH__SESSION_SECRET, …
env:
  - {name: FLOE__STORE__BUCKET, value: acme-floe}
  - {name: FLOE__STORE__S3__REGION, value: eu-west-1}
  - {name: FLOE__STORE__S3__ENDPOINT, value: "https://s3.eu-west-1.amazonaws.com"}
  - {name: FLOE__SERVER__PUBLIC_URL, value: "https://git.example.com"}
```

Behind ingress-nginx, git and LFS need the body-size, timeout and buffering annotations listed in
`values.yaml` (`ci/existing-secret-ingress-values.yaml` has them). For tmpfs caches as on the
reference hosts, give the `cache` volume `medium: Memory` and raise `resources.limits.memory` above
`cache.max_bytes`.

## TLS

| `[server.tls] mode` | In the cluster | Example |
|---|---|---|
| `off` (default) | an Ingress or HTTPRoute terminates TLS; set `server.public_url` | `ci/existing-secret-ingress-values.yaml` |
| `files` | floe serves a cert-manager `kubernetes.io/tls` Secret mounted at `/etc/floe-tls` (renewals reload without a restart); `healthCheck.scheme: HTTPS` | `ci/httproute-tls-files-values.yaml` |
| `acme` | floe orders its own certificate (DNS-01, Cloudflare); `CLOUDFLARE_API_TOKEN` and `FLOE_TLS_STORAGE_KEY` in a Secret; `/readyz` is 503 until the first certificate, so liveness/startup probe TCP | `ci/acme-values.yaml` |

## Split roles: a maintain host with an SSD

Roles (D9) and placement (D30) are per pod configuration. The library renders one Deployment per
release, so the split is two releases of this chart against the same bucket and Secrets:

```sh
helm install git          oci://…/floe -f ci/split-roles-serve-values.yaml      # roles serve+events, HPA, tmpfs
helm install git-maintain oci://…/floe -f ci/split-roles-maintain-values.yaml   # roles maintain, SSD PVC, no Service
```

The maintain release uses `persistence` (a ReadWriteOnce PVC mounted at `/var/lib/floe`,
`deploymentStrategy: Recreate`, one replica), `maintenance.disk = "ssd"`, `cache.mode = "disk"`,
and runs as the serving release's ServiceAccount (`serviceAccount.create: false`, `name: git-floe`)
so one IRSA role covers both. Placement globs (`FLOE__PLACEMENT__*`) are a group: setting any one
replaces the whole section.

## Metrics

`/metrics` is behind floe's auth. Add a read-only static token (principal `metrics`, `write =
false`), store it in the Secret and set `monitoring.endpoint.authorization`; `monitoring.kind`
picks ServiceMonitor, PodMonitor or VMServiceScrape (`ci/metrics-*-values.yaml`).

## Draining

SIGTERM starts D31's two-phase drain: up to 30 s of normal serving while maintenance stops, then
`server.drain_timeout` for in-flight requests with `/readyz` at 503. Keep
`terminationGracePeriodSeconds` above 30 s + `drain_timeout` (65 for the default 20 s).
