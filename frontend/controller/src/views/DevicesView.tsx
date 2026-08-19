import { Search } from "lucide-react";
import { useMemo, useState } from "react";

import { DeviceTable, EmptyDevices } from "../components/DeviceTable";
import { ErrorState, PageSkeleton } from "../components/ViewStates";
import type { Device } from "../types";

export function DevicesView({
  devices,
  error = null,
  loading = false,
}: {
  devices: Device[];
  error?: string | null;
  loading?: boolean;
}) {
  const [search, setSearch] = useState("");
  const filtered = useMemo(
    () =>
      devices.filter((device) =>
        `${device.hostname} ${device.os} ${device.agent_version}`
          .toLowerCase()
          .includes(search.toLowerCase()),
      ),
    [devices, search],
  );

  return (
    <div className="stack">
      <section className="section-intro">
        <div>
          <h2>Managed devices</h2>
          <p>
            Inventory, connectivity, and rollout state for every enrolled
            machine.
          </p>
        </div>
        <div className="search-box">
          <Search size={16} />
          <input
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search devices…"
            aria-label="Search devices"
          />
        </div>
      </section>
      <section className="card table-card">
        {loading ? (
          <PageSkeleton rows={5} />
        ) : error ? (
          <ErrorState message={error} />
        ) : filtered.length ? (
          <DeviceTable devices={filtered} />
        ) : (
          <EmptyDevices searching={Boolean(search)} />
        )}
      </section>
    </div>
  );
}
