// Temporary plain-text stand-in until ui/markdown.tsx lands.
export function Markdown(props: { text: string; streaming?: boolean }) {
  return (
    <div class="prose" style={{ "white-space": "pre-wrap" }}>
      {props.text}
    </div>
  )
}
