import { describe, expect, test } from "vitest"
import { canonical, expand, inputFor, joinPath, matchFolders, parentOf, splitInput } from "../src/views/folder-path"

const HOME = "/home/ada"

describe("expand", () => {
  test("tilde, absolute and bare names", () => {
    expect(expand("~", HOME)).toBe(HOME)
    expect(expand("~/", HOME)).toBe(HOME)
    expect(expand("~/repos/x/", HOME)).toBe("/home/ada/repos/x")
    expect(expand("/srv//app/./web/", HOME)).toBe("/srv/app/web")
    expect(expand("repos", HOME)).toBe("/home/ada/repos")
  })

  test("dot-dot climbs but never above root", () => {
    expect(expand("~/repos/../notes", HOME)).toBe("/home/ada/notes")
    expect(canonical("/../../etc")).toBe("/etc")
    expect(parentOf("/")).toBe("/")
    expect(parentOf("/home/ada")).toBe("/home")
  })

  test("no home yet falls back to root", () => {
    expect(expand("~/x", "")).toBe("/x")
  })
})

describe("splitInput", () => {
  test("lists the folder before the last slash, filtered by the rest", () => {
    expect(splitInput("~/repos/al", HOME)).toEqual({ dir: "/home/ada/repos", tail: "al" })
    expect(splitInput("~/repos/", HOME)).toEqual({ dir: "/home/ada/repos", tail: "" })
    expect(splitInput("/", HOME)).toEqual({ dir: "/", tail: "" })
    expect(splitInput("/et", HOME)).toEqual({ dir: "/", tail: "et" })
  })

  test("without a slash it searches home", () => {
    expect(splitInput("~", HOME)).toEqual({ dir: HOME, tail: "" })
    expect(splitInput("rep", HOME)).toEqual({ dir: HOME, tail: "rep" })
    expect(splitInput("", HOME)).toEqual({ dir: HOME, tail: "" })
  })
})

describe("inputFor", () => {
  test("shortens home and ends with a slash", () => {
    expect(inputFor("/home/ada/repos", HOME)).toBe("~/repos/")
    expect(inputFor(HOME, HOME)).toBe("~/")
    expect(inputFor("/home/adam", HOME)).toBe("/home/adam/")
    expect(inputFor("/", HOME)).toBe("/")
    expect(inputFor("/srv", "/")).toBe("/srv/")
  })

  test("round-trips through splitInput", () => {
    for (const p of ["/home/ada/repos", "/srv/x", "/"]) expect(splitInput(inputFor(p, HOME), HOME).dir).toBe(p)
  })
})

test("joinPath", () => {
  expect(joinPath("/", "srv")).toBe("/srv")
  expect(joinPath("/srv", "app")).toBe("/srv/app")
})

describe("matchFolders", () => {
  const names = ["alforria", "Notes", ".config", "api-alpha", "web", "libertai", "app10", "app9"]

  test("all visible folders, alphabetical, numbers in order", () => {
    expect(matchFolders(names, "")).toEqual(["alforria", "api-alpha", "app9", "app10", "libertai", "Notes", "web"])
  })

  test("prefix matches before substring matches, case-insensitive", () => {
    expect(matchFolders(names, "AL")).toEqual(["alforria", "api-alpha"])
    expect(matchFolders(names, "al").slice(0, 1)).toEqual(["alforria"])
    expect(matchFolders(names, "ert")).toEqual(["libertai"])
  })

  test("dot folders only when asked for", () => {
    expect(matchFolders(names, "co")).toEqual([])
    expect(matchFolders(names, ".")).toEqual([".config"])
  })
})
