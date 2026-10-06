"""A JSON Schema as the pydantic model `run_task` takes for `output_schema`.

`fastbrowse serve` gets its caller's output shape as JSON Schema draft 2020-12, since that is what a schema
library in another language can emit. Only keywords the model can enforce are taken. Any other is refused by
name, so a constraint the caller wrote is never dropped in silence.
"""

import keyword
import operator
import re
from collections.abc import Callable, Mapping
from typing import Annotated, Any, Literal

from pydantic import (
    AfterValidator,
    BaseModel,
    BeforeValidator,
    ConfigDict,
    Field,
    SerializerFunctionWrapHandler,
    Strict,
    create_model,
    model_serializer,
)

DRAFT = "https://json-schema.org/draft/2020-12/schema"

# How a value stands to each bound. In this draft `exclusiveMinimum` holds the bound and is no flag on `minimum`.
# Zod writes the two inclusive ones for every `z.int()`, so refusing them would refuse a plain integer.
_BOUNDS: dict[str, Callable[[Any, Any], bool]] = {
    "minimum": operator.ge,
    "maximum": operator.le,
    "exclusiveMinimum": operator.gt,
    "exclusiveMaximum": operator.lt,
}
_ANYWHERE = frozenset(
    {
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "const",
        "anyOf",
        "$ref",
        "description",
        "title",
        "default",
        *_BOUNDS,
    }
)
# `$defs` is read from the root alone, because `#/$defs/name` is the one form of `$ref` followed.
_ROOT = _ANYWHERE | {"$schema", "$defs"}
# `title` and `default` are annotations in JSON Schema: no validator holds a value to either, so taking them
# drops nothing the caller wrote. Zod writes them for `.meta({ title })` and `.default()`.
_NOTES = frozenset({"description", "title", "default", "$schema", "$defs"})
"""Keywords that say nothing about the value, and so may sit beside any other."""
_OBJECT = ("properties", "required", "additionalProperties")
_REFERENCE = re.compile(r"#/\$defs/(.+)")
# Pydantic would otherwise read "3" as an integer and "yes" as a boolean, which the caller's schema does not.
_SCALARS: dict[str, Any] = {
    "string": Annotated[str, Strict()],
    "integer": Annotated[int, Strict()],
    "number": Annotated[float, Strict()],
    "boolean": Annotated[bool, Strict()],
    "null": None,
}


class UnsupportedSchema(ValueError):
    """A schema that cannot become a model, with the keyword at fault and a JSON pointer to it."""

    def __init__(self, keyword: str, path: str, reason: str) -> None:
        super().__init__(f'"{keyword}" at {path}: {reason}')
        self.keyword = keyword
        self.path = path


class _Output(BaseModel):
    """The base of every model built here, which is what makes the data come back under the caller's names.

    A run fills the model by field name and dumps it with no arguments. A property whose name cannot be a
    field name is a field with an alias, so the model takes either name and writes the alias.
    """

    # `model_` is an ordinary start for a property name, and a property that would shadow an attribute of
    # the model is given another field name before pydantic could object to it.
    model_config = ConfigDict(
        validate_by_name=True, validate_by_alias=True, serialize_by_alias=True, protected_namespaces=()
    )

    @model_serializer(mode="wrap")
    def _present(self, handler: SerializerFunctionWrapHandler) -> dict[str, Any]:
        # A property left out of `required` may be absent, which is not the same as null: the caller's own
        # schema refuses a null where it allowed only a missing key.
        dumped = handler(self)
        for name, field in type(self).model_fields.items():
            if not field.is_required() and name not in self.model_fields_set:
                dumped.pop(field.alias or name, None)
                dumped.pop(name, None)
        return dumped


class _Closed(_Output):
    """An object whose schema says `"additionalProperties": false`."""

    model_config = ConfigDict(extra="forbid")


def output_model(schema: Mapping[str, Any]) -> type[BaseModel]:
    """The model for a JSON Schema whose root is an object. Raises `UnsupportedSchema`."""
    return _Conversion(schema).model()


def _pointer(path: str, token: str | int) -> str:
    return f"{path}/{str(token).replace('~', '~0').replace('/', '~1')}"


def _has_type(value: Any, name: str) -> bool:
    """Whether a JSON value is of a JSON Schema type. Python would call `True` an integer, and JSON does not."""
    match name:
        case "null":
            return value is None
        case "boolean":
            return isinstance(value, bool)
        case "integer":
            return isinstance(value, int | float) and not isinstance(value, bool) and float(value).is_integer()
        case "number":
            return isinstance(value, int | float) and not isinstance(value, bool)
        case "string":
            return isinstance(value, str)
        case "array":
            return isinstance(value, list)
        case _:
            return isinstance(value, dict)


def _same(left: Any, right: Any) -> bool:
    """Equality as JSON has it, where `1` and `true` differ at any depth."""
    if isinstance(left, bool) != isinstance(right, bool):
        return False
    if isinstance(left, list) and isinstance(right, list):
        return len(left) == len(right) and all(map(_same, left, right))
    if isinstance(left, dict) and isinstance(right, dict):
        return left.keys() == right.keys() and all(_same(value, right[key]) for key, value in left.items())
    return left == right


def _one_of(allowed: list[Any]) -> Callable[[Any], Any]:
    def check(value: Any) -> Any:
        if not any(_same(value, option) for option in allowed):
            raise ValueError(f"not one of {allowed!r}")
        return value

    return check


def _within(bounds: dict[str, Any]) -> Callable[[Any], Any]:
    def check(value: Any) -> Any:
        for key, bound in bounds.items():
            if not _BOUNDS[key](value, bound):
                raise ValueError(f"outside {key} {bound!r}")
        return value

    return check


def _usable(name: str) -> bool:
    """Whether a property name can be the field's own name."""
    return (
        name.isidentifier() and not keyword.iskeyword(name) and not name.startswith("_") and not hasattr(_Output, name)
    )


def _field_names(properties: Mapping[str, Any]) -> dict[str, str]:
    """A field name for each property: its own where that works, and otherwise the nearest name that does.

    The field name is what a run shows the models when it asks for the value, so it stays as close to the
    caller's as Python allows.
    """
    taken = set(properties)
    names: dict[str, str] = {}
    for name in properties:
        if _usable(name):
            names[name] = name
            continue
        field = re.sub(r"\W", "_", name)
        if not field[:1].isalpha():
            field = f"field_{field}"
        while field in taken or not _usable(field):
            field += "_"
        taken.add(field)
        names[name] = field
    return names


class _Conversion:
    def __init__(self, schema: Mapping[str, Any]) -> None:
        self._schema = schema
        definitions = schema.get("$defs", {}) if isinstance(schema, Mapping) else {}
        if not isinstance(definitions, Mapping):
            raise UnsupportedSchema("$defs", "#/$defs", "has to be an object of schemas")
        self._definitions = definitions
        self._resolved: dict[str, Any] = {}
        # The definitions being read, outermost first. One that turns up again refers to itself.
        self._resolving: list[str] = []

    def model(self) -> type[BaseModel]:
        root = self._annotation(self._schema, "#", "Output")
        if not (isinstance(root, type) and issubclass(root, _Output)):
            raise UnsupportedSchema("type", "#/type", 'a run returns an object, so the root has to be "type": "object"')
        # A definition nothing refers to is read as well, so an unsupported keyword in it is still refused.
        for name in self._definitions:
            self._definition(name, _pointer("#/$defs", name))
        return root

    def _annotation(self, schema: Any, path: str, name: str) -> Any:
        """The type for one schema. `name` is what a model built for it is called in a validation error."""
        if not isinstance(schema, Mapping):
            raise UnsupportedSchema(path.rpartition("/")[2], path, "has to be a schema object")
        allowed = _ROOT if path == "#" else _ANYWHERE
        for key in schema:
            if key not in allowed:
                raise UnsupportedSchema(key, _pointer(path, key), "this keyword is not supported")
        if path == "#" and schema.get("$schema", DRAFT) != DRAFT:
            raise UnsupportedSchema("$schema", "#/$schema", f"only {DRAFT} is supported")
        for note in ("description", "title"):
            if not isinstance(schema.get(note, ""), str):
                raise UnsupportedSchema(note, _pointer(path, note), "has to be a string")
        for alone in ("$ref", "anyOf", "enum", "const"):
            if alone in schema:
                self._alone(schema, path, alone)
        if "$ref" in schema:
            return self._reference(schema["$ref"], _pointer(path, "$ref"))
        if "anyOf" in schema:
            return self._nullable(schema["anyOf"], _pointer(path, "anyOf"), name)
        types = self._types(schema, path)
        if "enum" in schema or "const" in schema:
            return self._closed(schema, path, types)
        for key in _OBJECT:
            if key in schema and "object" not in types:
                raise UnsupportedSchema(key, _pointer(path, key), 'needs "type": "object" beside it')
        if "items" in schema and "array" not in types:
            raise UnsupportedSchema("items", _pointer(path, "items"), 'needs "type": "array" beside it')
        for key in _BOUNDS:
            if key not in schema:
                continue
            if not {"integer", "number"} & set(types):
                raise UnsupportedSchema(key, _pointer(path, key), 'needs "type": "integer" or "number" beside it')
            if not _has_type(schema[key], "number"):
                raise UnsupportedSchema(key, _pointer(path, key), "has to be a number")
        if not types:
            return Any
        annotation: Any = self._typed(types[0], schema, path, name)
        for other in types[1:]:
            annotation |= self._typed(other, schema, path, name)
        return annotation

    def _alone(self, schema: Mapping[str, Any], path: str, key: str) -> None:
        """Refuse a keyword that would be ignored beside `key`, which decides the value on its own."""
        beside = _NOTES | {key} | ({"type", "enum", "const"} if key in ("enum", "const") else set())
        for other in schema:
            if other not in beside:
                raise UnsupportedSchema(other, _pointer(path, other), f'is not supported beside "{key}"')

    def _types(self, schema: Mapping[str, Any], path: str) -> tuple[str, ...]:
        if "type" not in schema:
            return ()
        named = schema["type"]
        types = tuple(named) if isinstance(named, list) else (named,)
        known = ("object", "array", "string", "number", "integer", "boolean", "null")
        if not types or any(not isinstance(one, str) or one not in known for one in types):
            raise UnsupportedSchema("type", _pointer(path, "type"), f"has to be one or more of {', '.join(known)}")
        return types

    def _typed(self, type_name: str, schema: Mapping[str, Any], path: str, name: str) -> Any:
        if type_name in _SCALARS:
            scalar: Any = _SCALARS[type_name]
            bounds = {key: schema[key] for key in _BOUNDS if key in schema}
            if bounds and type_name in ("integer", "number"):
                # A validator and not the field's own bound, which would be written into the schema the models
                # are asked to fill, and a provider's structured output may refuse a schema that has one.
                return Annotated[scalar, AfterValidator(_within(bounds))]
            return scalar
        if type_name == "object":
            return self._object(schema, path, name)
        if "items" not in schema:
            return list[Any]
        item: Any = self._annotation(schema["items"], _pointer(path, "items"), f"{name}Item")
        return list[item]

    def _object(self, schema: Mapping[str, Any], path: str, name: str) -> type[BaseModel]:
        properties = schema.get("properties", {})
        if not isinstance(properties, Mapping):
            raise UnsupportedSchema("properties", _pointer(path, "properties"), "has to be an object of schemas")
        required = schema.get("required", [])
        if not isinstance(required, list) or any(one not in properties for one in required):
            # A required name with no schema could be anything, and a run has no way to fill it.
            raise UnsupportedSchema("required", _pointer(path, "required"), "has to be a list of names in properties")
        additional = schema.get("additionalProperties", True)
        if additional == {}:
            # The empty schema takes any value, which is what `true` says. It is how Zod writes a loose object.
            additional = True
        if not isinstance(additional, bool):
            raise UnsupportedSchema(
                "additionalProperties", _pointer(path, "additionalProperties"), "has to be true or false"
            )
        fields: dict[str, Any] = {}
        for (key, field_name), property_schema in zip(
            _field_names(properties).items(), properties.values(), strict=True
        ):
            at = _pointer(_pointer(path, "properties"), key)
            annotation = self._annotation(property_schema, at, key)
            field = Field(
                default=... if key in required else None,
                alias=None if field_name == key else key,
                description=property_schema.get("description"),
            )
            fields[field_name] = (annotation, field)
        return create_model(name, __base__=_Output if additional else _Closed, **fields)

    def _closed(self, schema: Mapping[str, Any], path: str, types: tuple[str, ...]) -> Any:
        """The values `enum` and `const` leave, of the types `type` leaves."""
        key = "enum" if "enum" in schema else "const"
        allowed = schema["enum"] if "enum" in schema else [schema["const"]]
        if not isinstance(allowed, list) or not allowed:
            raise UnsupportedSchema("enum", _pointer(path, "enum"), "has to be a list with a value in it")
        if "enum" in schema and "const" in schema:
            allowed = [value for value in allowed if _same(value, schema["const"])]
        if types:
            allowed = [value for value in allowed if any(_has_type(value, one) for one in types)]
        if not allowed:
            raise UnsupportedSchema(key, _pointer(path, key), "no value passes this together with what is beside it")
        # `Literal` takes no float, list or object, and it takes `True` for 1, so the check is made ahead of it.
        literal = all(value is None or isinstance(value, str | int) for value in allowed)
        values: Any = Literal.__getitem__(tuple(allowed)) if literal else Any
        return Annotated[values, BeforeValidator(_one_of(allowed))]

    def _nullable(self, branches: Any, path: str, name: str) -> Any:
        if isinstance(branches, list) and len(branches) == 2:
            for index, branch in enumerate(branches):
                if (
                    isinstance(branch, Mapping)
                    and branch.get("type") == "null"
                    and branch.keys() <= {"type", "description"}
                ):
                    return self._annotation(branches[1 - index], _pointer(path, 1 - index), name) | None
        raise UnsupportedSchema(
            "anyOf", path, 'only a schema and {"type": "null"} are supported, for a value that may be null'
        )

    def _reference(self, reference: Any, path: str) -> Any:
        found = _REFERENCE.fullmatch(reference) if isinstance(reference, str) else None
        name = found.group(1).replace("~1", "/").replace("~0", "~") if found else None
        if name is None or name not in self._definitions:
            raise UnsupportedSchema("$ref", path, "has to be #/$defs/ and the name of a definition in this schema")
        return self._definition(name, path)

    def _definition(self, name: str, asked_at: str) -> Any:
        if name in self._resolving:
            cycle = " -> ".join((*self._resolving[self._resolving.index(name) :], name))
            raise UnsupportedSchema("$ref", asked_at, f"recursive schemas are not supported ({cycle})")
        if name not in self._resolved:
            self._resolving.append(name)
            self._resolved[name] = self._annotation(self._definitions[name], _pointer("#/$defs", name), name)
            self._resolving.pop()
        return self._resolved[name]
