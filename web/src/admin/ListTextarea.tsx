import { useState, type TextareaHTMLAttributes } from "react";
import { listText, parseList } from "./schema-form";

/**
 * A textarea editing a list of strings (one per line, or comma separated).
 * It keeps the typed text while it still means the same list, so Enter or ","
 * at the end is not swallowed by the parse → join round trip; `onChange`
 * gets the parsed list on every keystroke.
 */
export function ListTextarea({
  value,
  onChange,
  ...rest
}: { value: unknown; onChange: (items: string[]) => void } & Omit<TextareaHTMLAttributes<HTMLTextAreaElement>, "value" | "onChange">) {
  const [text, setText] = useState("");
  const shown = listText(text, value);
  return (
    <textarea
      {...rest}
      value={shown}
      onChange={(e) => {
        setText(e.target.value);
        onChange(parseList(e.target.value));
      }}
    />
  );
}
