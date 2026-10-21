// 1-Wire driver (backend/crates/onewire): status shapes the panel reads.

export interface OwValue {
  k: string;
  label: string;
  v: number | string | null;
  d?: number;
  unit?: string;
}

export interface OwSlave {
  id: string;
  name: string;
  label: string;
  desc: string;
  family: string;
  present: boolean;
  romOk?: boolean;
  values: OwValue[];
  hist: Record<string, number[]>;
  pins?: { name: string; level: number; out: boolean }[];
  warn: string[];
  error?: string;
  module?: string;
  readAt?: number;
  firstSeen: number;
  eepromSize?: number;
  resolution?: boolean;
}

export interface OwMaster {
  id: string;
  label: string;
  kind: string;
  slaves: number;
  searchText: string;
  timeout?: number;
  pullup?: boolean;
  bulk?: boolean;
  ghosts?: number;
  error?: string;
}

export interface OwState {
  masters: OwMaster[];
  slaves: OwSlave[];
  interval: number;
  bulk: boolean;
  fixture?: boolean;
  root?: string;
  touchLog: { t: number; id: string; family: string; event: string }[];
  bridges: { type: string; bus?: number; address: string; note: string; bound?: boolean }[];
}
