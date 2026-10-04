import { useState } from "react";

type Props = { title: string };

export function App({ title }: Props) {
  const [count, setCount] = useState(0);
  return <button onClick={() => setCount(count + 1)}>{title} {count}</button>;
}

export const Header = (props: Props) => <h1>{props.title}</h1>;
