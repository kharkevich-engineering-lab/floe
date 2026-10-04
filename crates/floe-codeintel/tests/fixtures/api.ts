// API client (fixture).
export interface Repo {
  owner: string;
  name: string;
}

export type RepoId = string;

export enum Visibility {
  Public,
  Private,
}

/** Fetches one repository. */
export async function getRepo(id: RepoId): Promise<Repo> {
  const res = await fetch(`/api/repos/${id}`);
  return parse(res);
}

function parse(res: Response): Repo {
  return new RepoImpl(res.url);
}

export class RepoImpl implements Repo {
  owner = "";
  constructor(public name: string) {}

  /** The full name. */
  fullName(): string {
    return join(this.owner, this.name);
  }
}

const join = (a: string, b: string) => `${a}/${b}`;

namespace Legacy {
  export function old(): void {}
}
