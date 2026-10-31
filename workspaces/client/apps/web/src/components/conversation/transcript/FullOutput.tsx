import { useState } from "react";
import { Button } from "@mantine/core";
import { errorText } from "../../../api";
import { useServerApi } from "../../../servers/ServerServicesContext";
import { fullOutputText } from "./fullOutputCache";

/** Opens the full text of a cut item. It fetches by hash once, on the click. */
export default function FullOutput({
  blob,
}: {
  blob: { hash: string; size: number };
}) {
  const { get } = useServerApi();
  const [text, setText] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const open = async () => {
    setLoading(true);
    setError("");
    try {
      setText(
        await fullOutputText(blob.hash, async (hash) => {
          const response = await get("/api/transcript/blob", {
            query: { hash },
            timeoutMs: 15000,
          });
          return response.text;
        }),
      );
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      setLoading(false);
    }
  };
  if (text !== null) return <pre className="full-output">{text}</pre>;
  return (
    <>
      <Button
        size="compact-xs"
        variant="subtle"
        loading={loading}
        onClick={() => void open()}
      >
        Open full output ({Math.ceil(blob.size / 1024)} KB)
      </Button>
      {error && <p className="notice">{error}</p>}
    </>
  );
}
