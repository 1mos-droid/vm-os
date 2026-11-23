// Formalize HyperOS requirements with NASA FRET's own parser.
//
// Usage (from a FRET checkout's fret-electron directory, with its npm deps):
//   NODE_ENV=production node -r @babel/register <repo>/assurance/tools/formalize.js \
//       <requirements.json> <output.fcs.json>
//
// Writes a FRET "component spec" (the format FRET exports and Ogma reads with
// `--input-format fcs`): each requirement carries its FRETish text and FRET's
// past-time LTL (ptLTL, SMV syntax) and CoCoSpec formalizations.
'use strict';
const fs = require('fs');
const path = require('path');
const { compile } = require(path.resolve('app/parser/FretSemantics.js'));

const [input, output] = process.argv.slice(2);
if (!input || !output) {
  console.error('usage: formalize.js <requirements.json> <output.fcs.json>');
  process.exit(2);
}

const spec = JSON.parse(fs.readFileSync(input, 'utf8'));
const requirements = spec.requirements.map((req) => {
  const result = compile(req.fretish);
  const sem = result.collectedSemantics;
  if (!sem || !sem.ptExpanded) {
    console.error(`${req.id}: FRET could not formalize "${req.fretish}": ${result.parseErrors}`);
    process.exit(1);
  }
  return {
    name: req.id,
    fretish: req.fretish,
    ptLTL: sem.ptExpanded,
    CoCoSpecCode: sem.CoCoSpecCode,
  };
});

const doc = {
  [`${spec.component}_componentSpec`]: {
    Functions: [],
    Internal_variables: [],
    Other_variables: spec.variables.map((v) => ({ name: v.name, type: v.type })),
    Requirements: requirements,
  },
};
fs.writeFileSync(output, JSON.stringify(doc, null, 2) + '\n');
for (const r of requirements) console.log(`${r.name}: ${r.ptLTL}`);
