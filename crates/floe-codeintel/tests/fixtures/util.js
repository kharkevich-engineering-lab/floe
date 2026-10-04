// Utilities (fixture).
const path = require("path");

/**
 * Joins two segments.
 */
function joinAll(a, b) {
  return path.join(a, b);
}

class Cache {
  constructor() {
    this.map = new Map();
  }

  get(key) {
    return this.map.get(key);
  }
}

const double = (x) => x * 2;

module.exports = { joinAll, Cache, double };
