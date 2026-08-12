# Examples and quickstarts

Choose the deployment model first. Standalone examples run an Agent Desktop-owned local Gateway; managed examples use an organization bootstrap, browser identity, device approval, and a remote Gateway.

| Goal | Deployment model | Start here |
| --- | --- | --- |
| Develop the desktop UI with a local Gateway | Standalone | [Desktop UI standalone quickstart](../README.md#standalone-local-quickstart) |
| Develop the desktop UI through the managed identity journey | Remote managed | [Desktop UI managed quickstart](../README.md#remote-managed-quickstart) |
| Deploy enrollment, Gateway, administration, and monitoring on a VM or development laptop | Remote managed | [Managed server and client walkthrough](../docs/deployment/managed-vm-walkthrough.md) |
| Verify the connector and local Gateway without the UI | Standalone | [Container walkthroughs](../CONTRIBUTE.md#walkthroughs) |
| Run managed enrollment and forwarding automatically | Remote managed | [`scripts/managed-e2e.sh`](../CONTRIBUTE.md#automated-managed-e2e) |
| Inspect each managed API and approval transition | Remote managed | [Managed native walkthrough](managed-walkthrough/README.md) |
| Package an organization-specific installer | Remote managed | [Managed installer development](../docs/deployment/managed-installer.md) |

[`managed-organization.json`](managed-organization.json) is a production-shaped schema example. [`managed-walkthrough/organization.json`](managed-walkthrough/organization.json) is only for the disposable local fixture.
