# Gostly AI — Proxy

Stop writing mocks. Gostly sits in front of your external API calls, records real traffic, and replays it — so your team can build and test without depending on live services.

Recorded traffic stays on your machine — there's no Gostly cloud reading it.

## Quickstart

**1. Sign up and get your credentials**

Create an account at [gostly.ai](https://gostly.ai) and grab your license key and registry token from the dashboard.

**2. Authenticate with the Gostly registry**

```bash
docker login -u AWS -p <your-registry-token> 242201285974.dkr.ecr.us-east-1.amazonaws.com
```

**3. Configure `docker-compose.yml`**

- Replace `YOUR_LICENSE_KEY` with your key
- Set `BACKEND_URL` to the upstream service you want to mock

**4. Start**

```bash
docker compose up
```

The proxy is now running on port `8080`. Point your app at it instead of the real service.

## How it works

```
Your app → Gostly proxy (8080) → Real upstream service
```

In **LEARN mode**, Gostly records every request/response pair.  
In **MOCK mode**, Gostly replays recorded responses — no live service needed.

When Gostly doesn't have an exact match, AI fallback generates a plausible response based on learned traffic patterns (Pro/Team).

For a deeper walkthrough of the components, match pipeline, and storage model, see [ARCHITECTURE.md](./ARCHITECTURE.md).

## Ports

| Port | Service |
|------|---------|
| 8080 | Proxy (point your app here) |
| 3000 | Dashboard UI |
| 8000 | Control plane API |

## Docs

Full documentation at [gostly.ai/docs](https://gostly.ai/docs)

## License

A valid Gostly license key is required to run this software. [Get one at gostly.ai](https://gostly.ai).
