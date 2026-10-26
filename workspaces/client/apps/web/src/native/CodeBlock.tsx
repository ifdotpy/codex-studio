import CodeBlockView from "../components/CodeBlockView";
import { useNativePreview } from "./usePreview";
export default function CodeBlock(props: {
  source: string;
  language?: string;
}) {
  const preview = useNativePreview();
  return (
    <CodeBlockView
      {...props}
      copyText={(text) => preview.then((owner) => owner.copy(text))}
    />
  );
}
