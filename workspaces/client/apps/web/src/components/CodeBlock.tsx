import CodeBlockView from "./CodeBlockView";
import { copyText } from "../clipboard/clipboard";
export default function CodeBlock(props: {
  source: string;
  language?: string;
}) {
  return <CodeBlockView {...props} copyText={copyText} />;
}
