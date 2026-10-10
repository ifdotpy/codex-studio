import { useEffect, useState } from "react";
import { Button, NumberInput } from "@mantine/core";
import { get, errorText, post, type GetResult } from "../api";

type Configuration = GetResult<"/api/linux-vm/settings">;
const GiB = 1024 ** 3;

export function LinuxVMSettings({ active }: { active: boolean }) {
  const [configuration, setConfiguration] = useState<Configuration | null>(
    null,
  );
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (!active) return;
    let cancelled = false;
    void get("/api/linux-vm/settings").then(
      (value) => {
        if (!cancelled) {
          setConfiguration(value);
          setError("");
        }
      },
      (failure) => {
        if (!cancelled) setError(errorText(failure));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [active]);
  if (!active) return null;
  const fields = [
    { key: "cpus", label: "Processor cores", divisor: 1, min: 1, max: 256 },
    {
      key: "memoryBytes",
      label: "Memory (GiB)",
      divisor: GiB,
      min: 1,
      max: 64,
    },
    {
      key: "systemDiskBytes",
      label: "System disk limit (GiB)",
      divisor: GiB,
      min: 8,
      max: 128,
    },
    {
      key: "dataDiskBytes",
      label: "Workspace disk limit (GiB)",
      divisor: GiB,
      min: 8,
      max: 1024,
    },
  ] as const;
  return (
    <section className="settings-group" aria-label="Linux VM settings">
      <h2>Linux virtual machine (VM)</h2>
      <p>New Linux workers share this VM. Disk files use space as they grow.</p>
      <p>
        Stop the VM before you change its limits. Saved disks cannot shrink.
      </p>
      {configuration && (
        <>
          <p role="status">VM state: {configuration.state}</p>
          {fields.map(({ key, label, divisor, min, max }) => (
            <NumberInput
              key={key}
              label={label}
              min={min}
              max={max}
              allowDecimal={false}
              value={configuration.settings[key] / divisor}
              disabled={busy || configuration.state !== "stopped"}
              onChange={(value) => {
                if (typeof value === "number")
                  setConfiguration({
                    ...configuration,
                    settings: {
                      ...configuration.settings,
                      [key]: value * divisor,
                    },
                  });
              }}
            />
          ))}
          <Button
            mt="md"
            loading={busy}
            disabled={configuration.state !== "stopped"}
            onClick={() => {
              setBusy(true);
              setError("");
              void post("/api/linux-vm/settings", configuration.settings)
                .then(setConfiguration, (failure) =>
                  setError(errorText(failure)),
                )
                .finally(() => setBusy(false));
            }}
          >
            Save VM limits
          </Button>
        </>
      )}
      {error && <p role="alert">{error}</p>}
    </section>
  );
}
