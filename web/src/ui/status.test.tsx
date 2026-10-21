import { describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/preact";
import { AsyncButton } from "./common";

describe("<AsyncButton>", () => {
  it("shows its failure beside itself until dismissed", async () => {
    render(
      <AsyncButton
        onClick={async () => {
          throw new Error("no such master");
        }}
      >
        Search
      </AsyncButton>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Search" }));
    expect((await screen.findByRole("alert")).textContent).toBe("✗ no such master×");
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("shows `ok`, made from the result, when it works", async () => {
    render(
      <AsyncButton onClick={async () => ({ found: 3 })} ok={(r) => `Found ${r.found}`}>
        Search
      </AsyncButton>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Search" }));
    await waitFor(() => expect(screen.getByRole("status").textContent).toMatch(/^✓ Found 3 at /));
  });

  it("shows nothing after a plain success", async () => {
    render(<AsyncButton onClick={async () => {}}>Go</AsyncButton>);
    fireEvent.click(screen.getByRole("button", { name: "Go" }));
    await waitFor(() => expect((screen.getByRole("button", { name: "Go" }) as HTMLButtonElement).disabled).toBe(false));
    expect(screen.queryByRole("status")).toBeNull();
  });
});
